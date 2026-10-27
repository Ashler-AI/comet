# chat2: dumb-relay session sync + thin docs

Status: PLANNED · Author: 2026-08-09 investigation (whale-doc dissection + t3code comparison)
Prior art: `docs/registry-sync.md` (the same argument, applied to the workspace index).

## Why

Two compounding problems, one measured root cause:

1. **Session docs got fat.** The tool-output/diff caps (c951c3e, 2026-08-07) bound each
   *part*, not the *session*. Dissection of chat `1b65e93d` ("ACP Model Traits
   Integration Polish"): 1,079,986-byte snapshot, 10 messages, 426 tool parts —
   **917 KB (85%) is capped tool output**, history overhead 1.00× (pure payload, not
   oplog bloat). Every agentic session now reaches the ~1 MB wasm-wedge zone in a day.
2. **The SessionRoom DO wedges at that size.** Every incident class of Aug 4–5 (wasm
   heap poison, use-after-free doc wrappers, replay CPU-limit death, silent shallow
   exports, import penalty box) exists because the DO materializes the doc through
   loro-wasm. At 1 MB docs these fire routinely: the fresh-device symptom is
   join-OK → 3 silent rejoins → `REJECTED 3` (penalty box) → no backfill, forever.

Priority (product decision): **flawless sync on ~1.2 Mbps links beats tool-output
transparency.** Small docs first, unwedgeable serving second; neither substitutes for
the other (stripping shrinks bytes, the relay makes serving them instant and reliable).

## Non-goals

- Replacing Loro. Clients keep real Loro docs: same schema, same offline
  commit-then-converge, same command-ledger-as-outbox. Only the DO stops parsing bytes.
- Windowed/partial doc loads. Out of scope; the host-published tail sidecar covers
  first-paint latency.
- Preserving full tool outputs inside the synced doc. They move to a lazy sidecar.

---

## Workstream A — thin docs (ship first, independently)

Stops new whales being born. No protocol changes; additive on the existing s2 rooms.

**A1. Fold strips outputs/diffs to summaries.** In `crates/doc/src/parts.rs`:
- `output`: keep first non-empty line, ≤160 chars (t3code ships 84 and users cope).
  Add additive fields `outputRef: Option<String>` (sidecar key) and
  `outputBytes: Option<u64>` so the UI can render "Show full output (12 KB)".
- `diff`: replace inline `ToolDiff` text with per-file stats `{path, additions,
  deletions}` (t3's shape) + `diffRef`. Kill `TOOL_DIFF_DOC_CAP` usage — the 32 KB/edit
  inline diff is a bigger bomb than outputs, currently unexercised only because the
  claude harness emits none.
- Old readers: both fields are already serde-additive; old app versions render the
  summary as if it were the output. Acceptable.

**A2. Output sidecar.** *[PARKED 2026-08-10 (v0.1.30): product call — no R2
uploads for now. The fold keeps small outputs inline (≤160 chars,
fence-stripped) and summarizes big ones; full text survives only in the
host's run journal. The machinery below (blob routes, `sidecar_payload`,
`apply_sidecar_refs`, `upload_tool_sidecar`, UI upgrade path) is built,
tested, and dormant — reintroduction is re-adding one call site in
`sessions.rs`. M1's rebuild still returns sidecar payloads; the C3 cutover
must decide their fate (upload or drop) before flipping `roomGen`.]* Full (still 4 KiB-capped at the harness boundary) outputs and
diffs go to R2 through a Worker route (no DO involvement — the `BLOBS` bucket already
exists): `PUT/GET /blob/{chatId}/{partId}` , owner-auth via the existing Worker JWT
check. Host uploads are debounced/batched per commit tick, fire-and-forget (a lost
upload degrades to "full output unavailable", never blocks the doc). UI fetches on
expand; offline shows the summary + a greyed affordance.

**A3. UI.** Tool-part expansion fetches `outputRef` lazily; render summary inline.

Estimate: 2–3 days. Ships in the next release; immediately cuts new-session growth ~7×.

---

## Workstream B — chat2 edge room (dumb authenticated log relay)

New DO class `ChatRoom`, room name `chat2/{chatId}`, modeled line-for-line on
`RegistryRoom` (registry-room.ts, 425 LOC), not on SessionRoom. **No loro-wasm import
anywhere in the class.** With this, `edge/src/session-doc/` (the TS schema mirror) and
the wasm bundling alias eventually die with s2.

**Storage** (DO SQLite):
- `rows(seq INTEGER PRIMARY KEY, device TEXT, batch_id TEXT UNIQUE, bytes BLOB)` —
  opaque Loro update blobs. Per-row cap 1 MB (post-strip updates are KB-scale;
  oversized rows are rejected at the header, matching today's discipline).
- checkpoint blob via `blobs.ts` chunking + `meta`: `owner`, `seqFloor`,
  `checkpointFrontier BLOB` (opaque, client-written), `checkpointSize`,
  `checkpointSeq`, `tailDirty`.
- Sidecars: `tail` and `diff` blobs become **host-published** (`PUT /tail`,
  `PUT /diff`), served verbatim. The DO never materializes anything.

**Protocol** (binary WS frames: 1-byte type + JSON header + raw payload; no
loro-protocol, no base64 — 33% base64 overhead matters at 1.2 Mbps):
- `hello{cursor, device}` → `state{seqFloor, headSeq, checkpointSeq,
  checkpointFrontier, checkpointSize}` — metadata only, then:
- **Client-side precision** (replaces the server VV diff): client compares
  `checkpointFrontier` against its local doc frontiers.
  Included → skip the checkpoint, request `rows{after: max(cursor, checkpointSeq)}`.
  Not included → `GET /checkpoint` (HTTP, **Range-resumable** — a stored blob can
  resume at byte N; today's export-per-join cannot), then rows after `checkpointSeq`.
- `rows{after, excludeDevice}` → server streams rows `seq > after AND device !=
  excludeDevice` — you never re-download your own writes (matters exactly on the
  reconnect-after-offline-work path).
- `push{batchId, bytes}` → append + relay + `ack{batchId, seq}`. `batch_id UNIQUE`
  dedupes reconnect re-pushes server-side (client keeps a pending-unacked queue,
  registry-style; Loro re-import is a no-op so duplicates are safe end-to-end).
- `POST /checkpoint {seqCovered, frontier}` + chunked blob: owner-only, guarded by
  `seqCovered >= seqFloor` (floor-monotonic — the dumb replacement for the VV-monotonic
  R2 guard). Rows `seq <= seqCovered` deleted after commit.
- Presence: relay opaque ephemeral frames to live sockets with a 30 s TTL sweep —
  broadcast only, no EphemeralStore on the server.
- Validation kept (all wasm-free): auth/owner, frame shape, row size cap, per-device
  rate/byte quotas. Semantic garbage is contained per-user (owner-only rooms), skipped
  by client imports (malformed-entry philosophy), and erased by the next checkpoint.
- Ops: `GET /stats` (headSeq, seqFloor, rowBytes, checkpoint age), nightly
  seq-monotonic R2 backup (registry pattern), tombstone-free — rows are the log.

Estimate: 3–4 days including tests (registry-room test suite is the template).

---

## Workstream C — Rust client + host duties

**C1. `crates/sync/src/chat_client.rs`** modeled on `registry.rs` (764 LOC): cursor
tracking, pending-push queue with batch ids, hello/backfill/live loop, frontier
comparison for checkpoint skip, reconnect/backoff/wake plumbing reused from the
existing supervisor machinery. The layered liveness model (ping lease, join deadline,
probe clock) carries over — those lessons are transport-level, not CRDT-level.

Native local-update publication preserves commit order before rows enter the
durable chat outbox. The vendored Loro emitter stages each update while holding
the commit/oplog lock, then drains a serialized FIFO after releasing that lock.
Observer-triggered edits, reentrant callbacks and competing commit threads cannot
publish a dependent update ahead of its predecessor. Receivers still reject
incomplete causal history without advancing the durable cursor; historical
out-of-order rows require ordinary catch-up or a covering checkpoint.

The Edge also serializes each socket's inbound frames across asynchronous
authorization and dispatch; different sockets remain independent. Every frame
is reauthorized at the queue head. Active and queued input is bounded to 300
frames/8 MiB per socket and 1,200 frames/32 MiB per isolate. Close, revocation or
overflow discards queued input; active input stays charged until it settles.

**C2. Store migration** (`crates/sync/src/store.rs` MIGRATIONS — append entry #N):
`ALTER TABLE snapshots ADD COLUMN cursor INTEGER; ADD COLUMN epoch INTEGER` — cursor
persisted **in the same transaction** as the snapshot bytes, so content and cursor
cannot diverge (this kills the restored-backup/copied-device redownload cases at the
root).

**C3. Host duties** (`doc_host.rs`):
- Checkpoint policy: post a full checkpoint when server `rowBytes > 512 KB` or
  `rows > 200` (from `/stats` piggybacked on hello) — thresholds cheap to tune later.
  History trim: shallow checkpoint only at a frontier older than RETAIN_DAYS, same
  aged-frontier discipline as today (the ws4 live-frontier lesson: an offline device's
  concurrent ops must never land behind a shallow root).
- Publication order: a device's own ops go out in commit order. Loro runs local-update
  hooks after the commit, so two tasks committing at once can see them run swapped; a row
  whose causal history arrives later parks on every reader. `chat2_host::OwnOpsOrder`
  publishes the missing counter range from the doc first and drops the late hook.
- Tail sidecar: publish last-64 JSON on the debounced commit tick (dirty-flag, like
  the DO's current lazy recompute). Diff sidecar publish moves from `diff_sync.rs`'s
  DO PUT to the chat2 PUT unchanged.
- Non-host owner devices may checkpoint as fallback if floor lag exceeds a high-water
  mark (any device holds the full doc; ~20 lines, ships later if ever needed — hosts
  must be online to execute commands anyway, so lag is bounded in practice).

Estimate: 4–5 days.

**C4. iOS** (`apps/ios/Zeron/Sync/`): `ChatRoomClient.swift` cloned from
`RegistryClient.swift`; delete `LoroProtocol.swift` once s2 dies. Shared framing test
vectors across Rust/TS/Swift (registry precedent). Estimate: 3–4 days, can trail
desktop by a release.

---

## Migration + whale healing

The healing insight: **wedged s2 rooms are never repaired — they are bypassed.** The
host's local doc is the source of truth; the wedged DO simply stops receiving traffic
and ages out. This turns the incident-recovery problem into the migration problem.

**M1. Epoch rebuild (the healing step).** On first chat2 seed, the host does not
upload its fat doc — it **rebuilds a thin one**: fold every entry, strip outputs/diffs
to A1 summaries, upload the full texts it already holds to the A2 sidecar (existing
whale outputs stay viewable!), write fresh Loro doc with `meta.epoch = 2`. The
1 MB whale becomes ~160 KB with zero information loss. Commands: copy only unresolved
ledger entries (dedupe via `evaluate_command` + the mark-before-execute
`processed_commands` table protects against re-execution).

Migration 10 appends `processed_commands.outcome` to the unchanged first nine
migrations. The host saves a terminal command status/resolution before
acknowledging execution. A relay can finish before the sender-owned command row
arrives; the drain publishes the durable result on that row without re-executing
or copying the command. Existing schema-10 Boat stores retain their claims and
outcomes; older stores append the nullable column once. NULL outcomes retain
interrupted-execution rejection, never permission to replay.

In-process execution ownership follows the relay/drain future: cancellation
releases that ownership without clearing durable consumption. Preparation
receipts are observational, not an execution queue; under the same ownership
lock, the drain reconciles a saved result or rejects an interrupted claim and
publishes the watched transcript immediately. Local profile import carries
known outcomes and fills unknown results without replacing a destination's
known outcome. Legacy Crew imports keep their unknown-outcome semantics.

**M2. Cutover signal.** The registry chat row gains a `roomGen` field (per-field HLC
LWW like everything else): absent/1 = s2, 2 = chat2. The host flips it in the same
breath as seeding. Devices dial the room the registry names. No dual-write: at current
fleet scale (auto-updated within hours) a stale client seeing a frozen s2 doc until it
updates is acceptable; the field makes the cutover per-chat and instantly revertible.

**M3. Rebuild-vs-lineage on other devices.** A device opening a `roomGen: 2` chat whose
local doc has `epoch < 2` (or no doc): discard local doc **after** re-queueing any of
its own unresolved commands as fresh entries in the new doc, then adopt the chat2
checkpoint. Keep the old snapshot row (suffixed doc id, registry-migration precedent)
for rollback. Idempotent: epoch check makes re-entry a no-op.

**M4. Ordering.**
1. Release N: Workstream A (strip + sidecar). New sessions stop growing; nothing else
   changes. Bake ~2–3 days on the fleet.
2. Release N+1: B + C behind `roomGen`. Hosts migrate chats lazily — on next open or
   next command — not in a thundering herd. Wedged whales (the chats users actually
   notice) migrate the moment the user touches them: open → host rebuilds → seeds
   chat2 → flips registry → every device loads a 160 KB doc from a table-read DO.
3. s2 rooms: orphaned like ws3 before them. Nightly R2 backups already exist for
   forensics; delete the route after a deprecation window.
4. iOS in the following release; until then iOS reads `roomGen: 2` chats via the tail
   sidecar (read-only fallback it already has for thin clients) — or gate the flip on
   fleet capability if that's not acceptable.

**M5. Edge cases.**
- Host retired/dead, other devices hold the doc: first owner device to open the chat
  performs the seed (claim-on-first-seed; floor-monotonic guard makes a race benign —
  worst case two identical rebuilds, second checkpoint wins).
- No device holds the doc (only R2 backup of the wedged s2 room): operator path —
  extend `examples/doc_surgery.rs` to rebuild a thin doc from a backup blob and seed
  chat2 manually. Expected count: single digits.
- Mid-migration crash: seeding is (rebuild locally → POST checkpoint → flip registry).
  Each step idempotent; a crash before the flip leaves the chat on s2, retried next open.

**M6. Immediate relief (optional, pre-chat2).** For currently-wedged chats where the
user can't wait a release cycle: `POST /reset-log` on the s2 room + host re-push works
today but re-uploads the fat doc and will re-wedge. Only worth doing per-chat on
explicit request; the real healing is M1.

---

## Observability / acceptance

- `zeron sync` gains per-chat `cursor / headSeq / floorLag / pendingPushes`.
- Alert-shaped stat: any room with `headSeq - checkpointSeq` bytes > 2 MB or
  checkpoint age > 7 days (the passive failure mode this design trades into — make it
  visible from day one; silent truncation of the old wedge class must not become
  silent growth of a new one).
- e2e: extend `scripts/e2e-smoke.sh` — two engines, chat2 room, kill/rejoin mid-push,
  cursor resume, checkpoint-skip via frontier match, 1 MB fixture load under
  `tc`-throttled 1.2 Mbps (load must complete and be Range-resumable).
- Exit criterion mirroring the registry migration: `churn_stays_bounded` equivalent —
  N days of streaming on a chat2 room, rows never exceed the checkpoint threshold ×2.

### Current staging-native Edge smoke

`edge/scripts/smoke.mjs` exercises the actual Worker over loopback HTTP and
WebSockets, not the retired session/workspace protocol. It refuses remote hosts
and requires `AUTH_MODE=dev`, `ENVIRONMENT=local`, and project/deployment `local`.
It does not contact Scaffold or use real grants, release credentials, or cloud
storage. From `edge/`, install the locked dependencies with `npm ci`, then run:

```sh
npx wrangler dev --local --env staging \
  --ip 127.0.0.1 --port 27640 \
  --var AUTH_MODE:dev --var ENVIRONMENT:local \
  --var SCAFFOLD_PROJECT_SCOPE:local \
  --var SCAFFOLD_DEPLOYMENT_ID:local \
  --var SCAFFOLD_CONTROL_PLANE_URL:http://127.0.0.1:8788 \
  --persist-to "$(mktemp -d /tmp/crew-native-edge-smoke.XXXXXX)"
```

In another terminal, also from `edge/`:

```sh
node scripts/smoke.mjs http://127.0.0.1:27640
npx vitest run src/staging-routes.test.ts
```

The smoke checks two-peer registry backfill and bidirectional row convergence,
private-user isolation, two-peer chat2 opaque-byte backfill/live relay, ownership,
checkpoint and Range reads, and private R2 tool-blob round trips through
`/blob/:chatId/:partId`. It also checks missing/wrong-scope credentials, traversal,
and retired session/workspace routes. Native `/attachments/*` returns HTTP 410
(`legacy_attachments_retired`) after authentication: it never acknowledges and
discards an upload. Legacy migration attachment reads belong only to the separate
retirement adapter, not this native service.

Use fresh temporary persistence for each run; stop the local Worker and remove
only that owned temporary directory afterward. This is a synthetic transport
smoke, not a staging deployment, live migration, or inference verification.

## Sequencing summary

| Step | What | Size |
|---|---|---|
| 1 | A: strip outputs/diffs + sidecar | 2–3 d |
| 2 | B: ChatRoom DO + tests | 3–4 d |
| 3 | C: Rust client, store migration, host duties | 4–5 d |
| 4 | M: epoch rebuild, roomGen cutover, e2e | 2–3 d |
| 5 | iOS client | 3–4 d (trails) |

Roughly three weeks end-to-end with bake time, front-loaded so the bleeding (new
whales) stops in the first release.

## Imported original-epoch history publication

An offline Crew import queues ordinary registry upserts, but its retained chat
host is the absent original guest. Startup registry sync therefore publishes the
rows; the host-only checkpoint bootstrap cannot publish those cold snapshots.
Do not adopt the chats or start/recover their old executions to work around this.

After the offline export/import has finished into the fresh **owner controller's
selected Scaffold profile store**, start that existing native controller with its
isolated data root, current owner login, project/deployment, and native Edge:

```sh
comet --data-dir "$OWNER_DATA" --ipc-port "$PORT" \
  --edge-url "$NATIVE_EDGE" --scaffold-url "$CONTROL" \
  --project-scope "$PROJECT" --deployment-id "$DEPLOYMENT" headless
```

In another terminal, publish every explicitly selected terminal session:

```sh
comet --data-dir "$OWNER_DATA" --ipc-port "$PORT" \
  session publish-imported "$SESSION1" "$SESSION2"
```

`OWNER_DATA` is the installation root used for the fresh owner import, not the
`docs.sqlite3` store directory. This command uses that controller's local IPC;
it neither creates a guest grant nor reconnects the original physical sandbox.
Each selected ID invokes `PublishImportedCrewSession {logicalSessionId}`.
Authenticated DeviceRoom forwarding may additionally supply `targetDeviceId`.
No caller path, snapshot, proof, or report is accepted. Owner controller authority
is required locally and on the relay; sandbox/peer capabilities cannot publish.

The RPC first performs the same current-profile verification as
`VerifyImportedCrewSession`, requires an original-epoch marker, checks the exact
stored snapshot digest again, and sends those bytes through the native checkpoint
transport with `seqCovered=0`. It never opens, rebuilds, adopts, or executes the
session. An advanced room floor fails closed. It then waits up to 120 seconds
for the ordinary registry sync to acknowledge the original guest device ID,
session scope, and original-epoch marker. A timeout/error is not publication proof;
earlier successful uploads remain and the explicit command can be retried.
Imported source evidence and attachment copies/aliases remain in the owner profile.

The response is `{report, originalEpochHistoryProofJson}`. It is **not** the
control-plane activation receipt: the control plane independently invokes
`VerifyImportedCrewSession`, fetches native Edge `GET /chat2/:id/checkpoint`,
compares the raw body SHA-256 with `report.destinationSnapshotsSha256[id]`, and
fetches `GET /registry/:project/rows` to verify the original identity/scope/marker
before its generation-CAS. Never replace those reads with a caller report POST.

Focused checks (not part of the operator procedure):
`cargo test -p zeron-engine imported_checkpoint_posts_exact_bytes_and_rejects_advanced_floor`
and `cargo test -p zeron-engine imported_profile_verification`.
