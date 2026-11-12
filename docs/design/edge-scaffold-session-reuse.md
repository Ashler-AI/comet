# Edge: bounded reuse of Scaffold session verification

Status: prototype for review (2026-11-12). Not deployed. Branch `fix/edge-auth-session-grant`.

## Incident (2026-11-12, 22:06–22:16Z on the Mac; blips from 20:33)

Every Crew Staging engine on Christopher's Scaffold token lost sync at once: both Boat devboxes
(0.2.140) and the Mac (0.2.148). Engines logged `chat2/registry ... 503`, websocket 503s, and on
the devboxes `sync admission queue full`. `GET /auth/session` on the edge returned
`503 authority_unavailable` after ~8.03 s. That is the dispatch envelope (`SCAFFOLD_DEADLINE_MS`).

Evidence (Cloudflare GraphQL analytics, read-only; engine log `~/.crew-staging/logs/zeron-engine.log`):

| Signal | Normal (22:00–22:05) | Outage (22:06–22:15) |
|---|---|---|
| Edge Worker wall p50 | ~310 ms (200 ms is the coordinator rendezvous) | **8,000 ms** on every minute |
| Edge subrequests / request | ~2.0 (AuthGrant + room DO) | ~1.1 (stopped after auth) |
| NativeAuthGrant objects active | 1 (`afb3775f…`, one token shared by all devices) | same single object |
| NativeAuthGrant calls/min | ~1,150–1,650 | ~250–300 |
| Room DOs (chat/registry/device) wall p90 | ~0.6 s / ~0.6 s / ≤0.6 s | **8–19 s+** (per-frame `socketAuthorized` waiting on the same coordinator) |
| `scaffold-control-plane-staging` wall p50 | ~25 ms | 0.2–2.4 s, `scriptThrewException`, 60 s disconnects |
| Scaffold staging D1 query p50 | 0.2 ms | 0.2 ms (but ~70% fewer queries arrived) |

There was no deploy and no Cloudflare status incident, and the `scaffold-staging` GKE pods did not
restart. Scaffold's own D1 stayed fast. The degradation sat between the edge and Scaffold's Worker
(the GKE bridge path), and the per-request coupling turned it into a total sync outage. The
engine's own direct Scaffold calls also timed out (30 s) or returned 500 in the same window.

## 1. Where the edge consults Scaffold today (origin/main 13a4d6b48)

`src/index.ts` runs `authenticateResult` before routing **every** non-public request. For
`sc_rc_` bearers the path is: dispatcher `scaffoldAdmission("dispatch")` → AuthGrant
coordinator DO named `scaffold-verification:<sha256(token+config)>` → 200 ms rendezvous →
`GET {scaffold}/api/code-sandboxes/auth/session` (2 attempts × 2.918 s, 8 s envelope).

| Surface | Scaffold consulted |
|---|---|
| chat2 HTTP: rows (push/pull), tail, diff, checkpoint, stats, reset | every request |
| registry: rows, push, push-target, stats, reset | every request |
| device: status, sidecar, nudge; blob GET/PUT; directory-authority; releases | every request |
| websocket upgrade (chat2, registry, device, peer, preview) | at connect |
| **after connect** | `socketAuthorized` before **every inbound frame** and `sendAuthorized` before **every outbound batch** (`src/authority.ts`) |
| `/auth/device-grants` POST | bearer check plus a second Scaffold call (`comet-target/verify`) |
| `cs1.` device tokens | never Scaffold; an AuthGrant DO storage read per request |

**Cache or grant: none for `sc_rc_` tokens, by design.** The coordinator shares only in-flight
work. Its comments say so: "a rendezvous, not an authorization TTL", "No positive authority cache
survives the batch", and the tests asserted it. The only state that outlives a request is a
negative cooldown for `unavailable` (1 s, or the upstream Retry-After). So nothing could carry
clients through even a 1-minute outage. Sockets that had authenticated seconds earlier were
closed with 1013 on their next frame, and every reconnect and HTTP fallback got the 8 s 503.
`cs1.` device grants (12 h access TTL) do not use Scaffold, which is why sandbox devices behave
differently.

**Legacy vs native.** The legacy ae2 bundle, which in staging serves only `/api/releases/*` and
is the whole legacy production Worker, calls Scaffold directly from the request isolate. It has no
coordinator, rendezvous, admission budget or cooldown, and no per-frame socket revalidation for
Scaffold bearers. The native path funnels every request and frame for a token through **one** DO.
That is cheaper for Scaffold when healthy, but it adds 200 ms to every request and makes a single
object plus the isolate's 32/16 admission budget the choke point. Scaffold's per-token rate limit
(600/min) is also shared with the engine's own Scaffold calls.

## 2. Options

| | Revocation latency | Leaked token blast radius | Scope / capability checks | Tenancy | Memory | Client change |
|---|---|---|---|---|---|---|
| **A. Bounded reuse per token hash + serve-stale on authority failure** (recommended) | ≤ fresh (10 min) + one refresh while Scaffold answers; ≤ grace (24 h) after the last success only while Scaffold cannot | Unchanged while the token is live; revoked token works ≤ 10 min (≤ 24 h during an outage) | Re-evaluated on every refresh; a definitive denial or 401/403 purges at once, never stale | Key = existing coordinator name: SHA-256(token, URL, project, required caps, env, deployment) | ≤ 256 entries per isolate; one record per coordinator object | None |
| B. Edge-minted signed grant (HMAC/JWT), background refresh | Grant TTL (e.g. 5–10 min). Early revocation needs a lookup, which brings back a hop | Grant is a bearer for its TTL, narrower than the `sc_rc_` token | Baked into the grant until expiry; downgrade waits for TTL | Signature-bound; key rotation needed | None server-side | Engine + mobile + iOS; new Worker secret; old clients unprotected until updated |
| C. Verify only at websocket connect | Unbounded for long-lived sockets unless periodic checks are added | Socket lives until disconnect | Connect-time only | Fine | None | None. HTTP still fails during an outage |
| D. Negative caching of 401/403 only | Immediate | Unchanged | Unchanged | Fine | Small | None. No availability gain |

B is the right long-term shape if the coordinator hop itself proves fragile. It removes every hop,
but it is a cross-client protocol change and leaves today's 0.2.140 devboxes unprotected. A can
ship edge-only and is a prerequisite for B anyway. C fixes neither HTTP nor reconnects. D is
included in A as a 2 s rejection hold.

## 3. Prototype (option A)

`edge/src/scaffold-session.ts` holds the policy and the isolate LRU. `edge/src/auth-routes.ts`
holds the dispatcher and coordinator. `edge/src/auth.ts` classifies outcomes.

- **Fresh (10 min):** isolate hit costs no hop; coordinator hit costs no Scaffold call. The 200 ms
  rendezvous is gone; single-flight refresh remains.
- **Past fresh:** one shared coordinator consultation per isolate and token (2 s deadline).
  The coordinator waits ≤ 1 s for its single-flight Scaffold refresh. If Scaffold answers, that
  answer is authoritative. If not, it serves its record. A refresh of a reusable record outlives
  its callers (≤ 8 s) and renews the record when it lands.
- **Unavailable** (transport, timeout, 408/429/5xx, admission, coordinator unreachable from a
  warm isolate): serve the last verification until `verifiedAt + 24 h`, never extended by
  serving. The isolate then waits 5 s before asking again.
- **Definitive:** 401/403 → purge plus `invalid` (401). A decoded 2xx that fails the resource,
  actor or required-capability checks is the new internal `denied`. It purges, keeps the public
  503 (unchanged contract), and is never served stale. Both hold for 2 s to collapse bursts.
- **Durability:** the coordinator persists its one record (`scaffold-session` key) with an alarm
  at the grace bound. Restarts keep the bound; rejection deletes it.
- **Mid-deploy:** a positive reply without bounds (old coordinator) authorizes that request only.
- Knobs `SCAFFOLD_SESSION_FRESH_MS` / `SCAFFOLD_SESSION_GRACE_MS` are optional and clamped
  (fresh 100 ms–1 h, grace fresh–7 d). Unset means 10 min / 24 h.
- One fixed-field `console.warn` per failed refresh while reusing (no subject or token), for
  `wrangler tail`.

Stated bounds: revocation or capability downgrade takes effect ≤ ~10¼ min worst case (10 min
fresh + 1 s coordinator wait + 5 s isolate recheck + one ≤ 8 s refresh), for HTTP and open
sockets (4401). While Scaffold cannot answer, a revoked token keeps working until 24 h after its
last successful check, and dies at the first refresh once Scaffold answers again (refreshes honor
its Retry-After). An unreachable authority closes sockets 1013 and returns 503 only after grace.

Why these values (internal tool). Fresh only bounds revocation and downgrade while Scaffold is
healthy. Grace is what carries clients through an outage. Scaffold load is trivial either way:
about one call per active token per fresh window, against ~60/min before. Raising fresh mainly
shrinks exposure to a slow coordinator hop, because warm isolates skip the hop for the whole
window. What revocation protects is real but owner-scoped. A full user token reaches only its
owner's device rooms, but there it may send unrestricted `rpc` and `term` frames, which means
remote control of that person's Mac and devboxes. `sc_rc_` tokens never expire, so revocation is
the only kill switch. Minutes of delay is an acceptable price for an internal tool; hours of grace
apply only during an authority outage.

Tests (`npm run test:unit`, `test:workerd`, staging tier): unit 182, workerd 56, staging 11, all
passing. New coverage: `src/scaffold-session.test.ts` and two real-binding workerd cases (outage
until grace with live sockets; capability downgrade closing a socket). Existing tests that
asserted "no positive TTL" were updated to the new bounds. Mutation checks confirmed the new
tests fail if the stale fallback, the purge on revocation, the denial classification, the grace
bound or the detached refresh are broken.

## 4. Engine

Not required for this fix: it is edge-only and helps every client version. Recommended
separately, because the engine amplified the outage:

- Treat edge `503 authority_unavailable` (and close 1013) as **one process-wide** condition that
  honors `Retry-After` with jitter. Today every chat2/registry client backs off alone
  (250 ms → 16 s), ignores `Retry-After`, and a single sibling success wakes them all at once.
- Do not `spawn_offline_sync()` an HTTPS pull/push after a dial fails on authority unavailability:
  it fails the same way and doubles load.
- Fail admission fast while that condition holds, instead of queueing up to 128 waiters behind
  8-second 503s (`crates/sync/src/budget.rs`).

## Open decisions

1. Fresh 10 min / grace 24 h (relaxed from 60 s / 15 min for an internal tool). Grace bounds
   how long a revoked token keeps working during an outage, including one an attacker pushes into
   429 at Scaffold. The token's last real verification still anchors it.
2. Kill switches. With a long grace, making Scaffold's `/auth/session` *unavailable* no longer cuts
   edge access promptly. That covers turning off remote code (Scaffold returns 404
   `remote_code_sandboxes_disabled`, classified as unavailable) or taking the bridge down.
   Revoking the token (403) still does, within fresh. If an "off" switch must be immediate, map that
   specific 404 to `denied`.
3. Whether `denied` should become a public 403 instead of today's 503 (client-visible change).
4. Cold isolates still need the coordinator hop. If that hop is what degraded (the 8 s here was
   at the dispatcher while the DO's own wall time stayed short), only option B removes it.
