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
   - Verify a fresh native session and a legacy resume.
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
