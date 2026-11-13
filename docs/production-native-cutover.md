# Native Crew production cutover

Production still runs legacy Crew 0.1.156 (`scaffold.comet-runtime.v1`) on the
legacy Edge Worker `ashler-comet-edge-production` at `comet.internal.ashler.com`.
Staging runs native Crew. This is the plan for moving production to native without
breaking legacy Scaffold sessions that are still in use.

## Who uses production (measured 2026-11-12, read-only)

Cloudflare analytics for `comet.internal.ashler.com`, 2026-11-05 to 2026-11-12:

| Client | Requests | What it is |
| --- | --- | --- |
| `35.237.177.159` | 1.62M | NAT of `ashler-e2b-dev-egress-router`, i.e. production Scaffold sandboxes |
| `2a06:98c0:3600::103` | 2.3k | Scaffold control-plane Worker (745 `POST /auth/device-grants`) |
| `69.193.150.98` | 112 | One device on the office network, on 11-12 only, failing status polls |
| 36 other IPs | 3.4k | Scanners (`/admin`, `/login`, …) |

No desktop or mobile client used production outside the office. The production
Scaffold D1 (`scaffold-metadata`) had 744 sessions created or active in that
window. 92 of them are `comet_remote` sessions that can still resume
(paused/ready/starting/restoring), spread over six users. Scaffold stops paused
sessions after 7 days, so those 92 age out on their own.

## Design

Native production is a separate Worker on a separate origin:
`ashler-crew-edge-production` at `crew.internal.ashler.com` (`edge/src/production.ts`,
`env.production` in `edge/wrangler.jsonc`). The legacy Worker is not deployed or
modified.

- **Why not replace legacy in place, as staging did?** Legacy and native mint the
  same credential shapes (`cs1.` device grants, `sc_rc_` Scaffold tokens), so one
  Worker would have to consult two grant authorities to admit both. A paused legacy
  sandbox resumes with its 0.1.156 binary, which needs legacy routes that the native
  Edge answers with 410. Keeping two origins keeps every legacy session working until
  it ages out, with no migration. No legacy-to-native session migration exists for
  production.
- **Shared resources.** The native Worker shares the production R2 bucket. Native
  access is fenced to `native-v2/`; legacy writes only `att/` and `backup/`. Release
  reads (`GET|HEAD /api/releases/<file>`) go over the `LEGACY_EDGE` service binding to
  the legacy Worker, which remains the only GCS release reader and authenticates the
  caller itself. No secret is copied.
- **Desktop.** The packaged production app (`apps/zeron/src/environment.rs`) dials
  `crew.internal.ashler.com` and keeps its own `~/.crew` root. It starts fresh: legacy
  `~/.comet-native` is retained, not imported. No legacy desktop client is active.
- **Not wired yet.** No APNs key is bound, so notifications are logged, not sent.
  Native sandboxes' `cs1.` release reads are refused by the legacy route, the same as
  on staging.

## Order of operations

Each production step needs point-of-execution approval. Both `comet-production`
(Edge) and `comet-release-production` (release) require a reviewer.

1. **Land this change, then deploy the native production Edge.** Run
   `deploy.yml --ref main -f source_sha=<merged sha> -f target=production`. The
   `comet-production` environment needs `CLOUDFLARE_API_TOKEN` and
   `CLOUDFLARE_ACCOUNT_ID` secrets. The job checks `/health` on the new origin.
2. **Release the next workspace version to staging** and verify the signed Crew
   Staging app.
3. **Publish an immutable production candidate.** Run `release.yml` with
   `promotion_target=production-candidate`, `release_surface=desktop-and-scaffold` and
   `candidate_run_id=<staging run>`. This writes only
   `releases/<version>/*` to `ashler-comet-releases-production`. No channel, alias or
   Scaffold manifest moves, and the runtime guard is not consulted. This gives primary
   Scaffold real v2 bytes to build from before production is acknowledged.
4. **Primary Scaffold, in reviewed `ashler-platform` PRs plus an approved primary
   dispatch:**
   - Give each session generation its own Edge URL. Legacy `session` sessions keep
     `comet.internal.ashler.com`; `chat2` sessions use `crew.internal.ashler.com`.
   - Set `SCAFFOLD_CREW_SYNC_GENERATION=chat2` for new sessions.
   - Build the `chat2` template from the step 3 tuple (version, production bucket,
     Linux SHA-256) while existing legacy sandboxes keep resuming.
   - The provider must hand sandboxes Scaffold's public https origin
     (ashler-platform #13388). Primary's own control-plane URL is cluster-private, and
     native Crew refuses it.
   - The Crew release must include #215: crew-web must present the engine's IPC
     token, or every native sandbox stalls with crew-web at 503.
   - Set the pins in both places. The primary build job runs in the `staging` GitHub
     environment, and its `SCAFFOLD_ASHLER_COMET_VERSION` and
     `SCAFFOLD_ASHLER_COMET_LINUX_SHA256` override the repository variables.
   - Flip the template and the Worker back to back, at a quiet time. Each provider pod
     runs one template generation. Until the `chat2` Worker is deployed, new Crew
     sessions are refused with `crew_session_migration_required`. Prebuild the images
     and template, materialize the lock, and deploy the Worker as soon as the new
     provider pods take traffic.
   - Verify a fresh native session and a legacy resume. If a native session stays
     `starting`, read `/workspace/.scaffold/logs/comet.log` in its sandbox.
5. **Promote production.** Run `release.yml` with `promotion_target=production`,
   `scaffold_runtime_deployment=production-deployed` and the same `candidate_run_id`.
6. **Verify.** Check production desktop login and `update --check` against
   `crew.internal.ashler.com`, a new native Scaffold session, and a legacy paused
   session resume.
7. **Retire the legacy Worker** in a separate change, after the last legacy session has
   stopped.

Everything up to step 4 is additive and invisible to existing users. Step 5 moves
production channels, and they never move backward, so recovering after step 5 means a
newer forward release.

## 2026-11-12 attempt: Scaffold rolled back, desktop shipped ahead

Steps 1–3 completed. Step 4 rolled every primary Scaffold service, the sandbox
provider and the template forward from `master`. From 21:40Z to 22:31Z, no new Crew
(`comet_remote`) session became ready. There were two causes:

- **Skew, 21:40–21:49Z.** The first run's lock (`f2118967eb`) rolled the `chat2`
  provider and the native-only template but kept the legacy Worker. Every new Crew
  session was created as `session` and refused with `crew_session_migration_required`.
  This lasted until the second run deployed the `chat2` Worker.
- **Native Crew could not authenticate, from 21:49Z.** The provider passed its own
  cluster-private URL (`http://scaffold-control-plane.scaffold.svc.cluster.local`)
  into sandboxes as `SCAFFOLD_CONTROL_PLANE_URL`. The supervisor gives that URL to the
  engine as `--scaffold-url`, and the engine accepts only an https origin. It logged
  `Scaffold auth is not configured correctly` and exited, so crew-web stayed at 503
  and startup health never passed. Staging's control-plane URL is already public,
  which is why staging never showed this. ashler-platform #13388 fixes it.

The coordinator was not overloaded. SessionCoordinator Durable Objects stayed near 2×
baseline, with p99 around 100 ms, while sessions were already failing. A `remote_code`
canary became ready at 22:16Z. Separately, from 22:06 to 22:15Z, client traffic to the
Worker and the provider dropped about 75%, and the Worker threw exceptions. That dip
is still unexplained, so watch for it on the retry.

ashler-platform #13360 restored the previous lock, and the control-plane Worker was
rolled back to its legacy-config version. #13368 reverts the primary `chat2` config
until a retry.

The desktop app ships ahead of Scaffold instead: `release_surface=desktop`,
`scaffold_runtime_deployment=desktop-ahead-of-scaffold`, reusing the staging candidate
run. Production Scaffold stays on the v1 runtime.

## 2026-11-13 retry: rolled back, third cause found

The retry shipped #13388 with Crew 0.2.148. The template flipped at 19:08Z, and the
Worker flipped at 19:14Z after a 5-minute skew window. #13388 worked: the engine started
with `--scaffold-url https://scaffold.internal.ashler.com` and stayed up. But a fresh
native session still never became ready. The 0.2.148 engine refused crew-web's IPC
connections ("rejecting IPC handshake without the engine's bearer token", 401). Since
`454c93e72` the engine requires `Authorization: Bearer <{data_dir}/ipc-token>`, and
crew-web's `CrewRpc` never sent it. The supervisor's engine probe and agent bridge import
the same client. #215 fixes that.

Recovery:

- The Worker was rolled back to `f4925073` at 19:25Z. ashler-platform #13524 restored the
  pre-flip lock and reverted the `chat2` config.
- A new Crew session was ready 28 s after launch on the legacy stack (19:42Z).
- No user session was affected; only the test session was created in the window.

The next retry needs a Crew release that includes #215.

## 2026-11-13 0.2.149 attempt: rolled back, cutover blocked on a design gap

0.2.149 (#215, #216) flipped at 21:31Z, and #215 worked: the engine came up and
crew-web reached it over IPC. But no new native session became ready. The engine logged
"directory update awaits synchronization or reconciliation … directory awaits
acknowledged registry row", and `WatchChats` returned no rows. crew-web never found its
session context, so it stayed at 503.

A `chat2` sandbox only hosts a chat that already has a row in the native Edge's org
registry. Only a native Crew client creates that row: it creates the chat, then calls
`POST /api/code-sandboxes` with that chat id. Production sandboxes mostly come from other
creators, none of which create the row:

- agents through the Scaffold MCP (`/mcp/code-sandboxes`; the origin defaults to `ui`);
- legacy clients;
- the Scaffold web UI's "Launch sandbox".

Staging never showed this, because every staging native session came from the native
Crew app.

Recovery:

- The Worker was rolled back at 21:36Z, and ashler-platform #13594 restored the lock.
- A new legacy session was ready at 21:53Z.
- One user session created in the window stalled.

**Before any retry, decide how sessions from non-native creators get a registry row.**
For example, the Worker could create the chat server-side on the native Edge, or those
sessions could stay on the legacy generation, which needs both templates.
