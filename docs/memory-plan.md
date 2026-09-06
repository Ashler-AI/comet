# Memory plan — bounded RSS without touching feel

Written 2026-08-03 after a full-codebase audit plus empirical benchmarks (below).
Problem: Activity Monitor shows Comet at 450–600MB on viewer-only laptops and
>1GB after heavy use. Target: ~150–250MB steady-state, flat over a workday.

## 1. What we measured

Engine benchmarks ran the real `comet headless` debug binary (v0.1.12) with the
mock harness, offline, on Linux/glibc; RSS sampled from `/proc`. Scripts:
`/tmp/comet-mem-test/{run,bench2,bench3}.sh` (to be promoted to
`scripts/mem-smoke.sh`, phase 0).

| Measurement | Result |
|---|---|
| Engine idle, no docs | **32MB** |
| Boot with 6 chats on disk (docs lazy-load) | 35MB — nothing opens at boot |
| Retention per streamed chat (~330KB text) | +1.4–7.7MB, **never released** (0 bytes back after 60s idle) |
| Retention per reopened chat (no streaming) | +1.2–2.8MB — the rest of the streamed number is allocator watermark |
| Streaming 1.6MB-text chat | **+18.6MB resident (≈11.6× raw text)** |
| Idle creep, 6 docs open, 3min | +0.2MB — growth is event-driven, not timer-driven |
| Cold-open small chat (snapshot→first frame) | **18ms** (warm re-watch: 17ms) |
| Cold-open 1.6MB chat | **62ms** (warm: 51ms) — delta ≈ 11ms, debug build |
| Watch frame size (what's re-serialized per 120ms tick, ×~4 copies) | 227KB small chat / 1.13MB big chat |
| Loro snapshot on disk | 6–13KB (columnar+compressed; mock text overly compressible — treat as lower bound) |

Code-audit findings backing each work item are cited inline (file:line refs
verified 2026-08-03).

**The two load-bearing empirical facts:**
1. Cold-open from the SQLite snapshot is within ~11ms of a warm doc, even for a
   large chat, even in a debug build. Doc eviction is therefore *not* a feel
   trade — the speed of opening chats comes from having local state on disk,
   not from unbounded residency.
2. Nothing is ever released, so RSS is monotonic in chats-ever-touched,
   images-ever-viewed, and terminals-ever-opened — plus a malloc watermark
   (macOS libmalloc never returns small-span pages) fed by ~4 full-transcript
   copies per 120ms streaming tick.

## 2. Feel budgets (regressions gated on these)

- Chat switch between recent (warm) chats: unchanged — they stay pinned.
- Reopen of an evicted chat: first paint < 100ms (measured 62ms debug worst
  case today); live deltas may land up to ~200ms after paint (one room RTT).
- Streaming smoothness: unchanged or better (phase 2 removes per-tick work).
- Reattach-after-detach: unchanged — engine keeps hosting docs
  it has run/command obligations for.

## 3. Phase 0 — measurement first

- Promote the bench scripts to `scripts/mem-smoke.sh`: boot baseline, N-chat
  stream, reopen latency, idle-flatness; assert thresholds; run in CI (Linux).
- macOS one-pager in the script header: `footprint -p <pid>` / `vmmap
  --summary` to split MALLOC vs IOSurface/Metal when a report comes in.
- Every phase below lands with before/after mem-smoke numbers in the PR.

## 4. Phase 1 — zero feel risk

| Item | Change | Evidence | Expected |
|---|---|---|---|
| Allocator | mimalloc (or jemalloc) as global alloc in `apps/comet` | system malloc watermark; churn sources below | watermark becomes recoverable; biggest single lever on macOS |
| Image lifecycle | Call gpui `remove_asset` when transcript rows drop / adopt an LRU image cache with byte budget; bound the global encoded-bytes cache `attachments.rs:462` (no eviction today); clear staged attachments on chat delete | decoded RGBA + atlas tile + encoded bytes ≈ 2.5× decoded size per image, permanent; one screenshot ≈ 48MB decoded | 100MB+ on image-heavy use |
| Doc delete eviction | `DeleteChat`/`DeleteSpace` drop the doc handle, close the room, delete the snapshot row (`doc_host.rs:125` handles map is insert-only; `rpc.rs:636-693` leaks) | audit §1 | correctness + a few MB per deleted chat |
| Bound channels | `RpcClient::subscribe` unbounded (`client.rs:123`) → conflating/bounded (watch semantics are latest-wins); terminal PTY + subscriber channels (`terminals.rs:211,243`) → bounded with drop policy; offline local-update queue (`room.rs:341`, drains only on connect) → byte cap + full-resync on overflow | leak-shaped under slow consumer / disconnect | removes the balloon modes (dev builds, sleep/wake, firehose terminals) |
| Hygiene | clear codex `streamed_text` per turn; evict idle journal fds; prune `dial_locks` | audit §3/§9/§7 | small, stops slow creep |

## 5. Phase 2 — bounded docs + delta streaming (the structural fix)

1. **Doc LRU** — wire the dead `DOC_LRU_BYTE_BUDGET` (80MB,
   `crates/doc/src/constants.rs:17`). Pins: selected chat; chats this device
   hosts with a live run or undrained commands; N most-recent. Evict = flush
   snapshot (already 1Hz-debounced), drop `ChatDocHandle` + mirror, close
   room. Measured reopen cost: +11ms vs warm. This alone caps the growth term
   that took the home laptop to 557MB with zero local sessions.
2. **Lazy mirror** — `messages_tx` holds a full transcript copy per open doc
   even with no subscriber (`doc_host.rs:142,229`); materialize only while a
   watch is attached.
3. **Delta doc-watch** — `WatchDocMessages` currently re-serializes the whole
   transcript per 120ms commit through 4 copies (`engine/src/rpc.rs:775` →
   `rpc/src/client.rs:139` → `ui/state.rs:902` → `transcript.rs:1199`;
   measured 1.13MB/frame on a 1.6MB chat). Send per-entry deltas. Also fixes
   streaming CPU — same pipeline as the remote-streaming chunkiness work.
4. **Fold O(n²)** — `fold_event_into_parts` clones the whole parts vec per
   event (`parts.rs:85`); mutate in place. `render_parts` clone per tick
   (`sessions.rs:805`) → borrow.
5. **Incremental reads** — `read_entries`/`read_commands` do whole-doc
   `get_deep_value().to_json_value()` per tick (`schema.rs:209,233`); move to
   `doc.subscribe` diff application (the mirror layer's stated design,
   ARCHITECTURE.md §2.3). Same for `workspace.rs:291` `chat()` linear
   whole-container scan on every 120ms `is_host` check.

Expected after phases 1–2: streaming multiplication ~11.6× → ~2–3× raw text;
viewer-laptop steady state ≈ gpui baseline + selected chat ≈ 150–250MB, flat.

## 6. Phase 3 — larger surfaces, more care

- **Terminals**: purge `terminal/panel.rs:239` chats map on chat delete;
  scrollback 10k → configurable ~2k lines (24B/cell ⇒ 30–50MB per
  fully-scrolled terminal today); count replay bytes raw, not base64.
- **Diff pane**: summary-only watches plus exact-checksum on-demand fetch for the
  selected checkout have landed; the UI retains one patch instead of every
  checkout's ≤3MiB patch. Remaining: stop the 120s repair tick re-capturing
  unchanged checkouts (`diff_sync.rs`).
- **Transcript render caches**: byte-budget `tree_cache`/`RenderCache`/
  `HighlightStore` to viewport±K rows (today they grow with every row ever
  scrolled past, freed only on chat switch).
- **Shallow snapshots** (deferred, correctness-sensitive): client-side trim to
  the edge's compaction frontier would cut in-memory doc 2.5–4× → ~1×; needs
  the stale-peer story (`room.rs:132` gives up rather than rebuilding).
- **Tail-first cold open** (`materialize_tail` exists unwired,
  `schema.rs:738`): paint last-64 for never-opened remote chats while the doc
  backfills. Perceived-latency win, not a memory item.

## 7. Implementation status (2026-08-03)

Phases 1–2 landed on `comet/comet-memory-usage-investigation` (phase 3
deliberately skipped — terminals get their own pass with the terminal bug
work). Shipped: mimalloc in both binaries; attachment-image LRU (64MB encoded
budget) with gpui asset release on eviction + staged-attachment purge on chat
delete; doc eviction on DeleteChat/DeleteSpace; doc LRU (12 warm docs / 80MB
estimate, pinned: watched, live-writer, host-pending-commands) with lazy
mirror; in-place event fold; container-scoped doc reads + single-row workspace
`chat()`; delta `WatchDocMessages` protocol (reset + per-entry upserts + text-append
ops, desync → resubscribe) across engine/UI — measured on a 1.6MB
streamed reply: 257MB of watch frames before, 2.3MB after (110×; median
frame 2.7KB, one full-entry frame at turn completion); bounded RPC stream queues (256,
backpressure); offline room queue drained during backoff; codex/journal-fd/
dial-lock hygiene. Verified: full workspace suite green; 20-chat run shows the
LRU evicting (8×), post-cap growth slope halved, and RSS recovering at idle —
which the baseline never did. Cold-open stayed on the measured ~62ms path.

Known follow-ups: GPU atlas tiles for raw-bytes images still free only on
window close (needs a small gpui-fork patch exposing a drop path for
`ImageSource::Image`); UI-side full-transcript clone per frame
(`transcript.rs` sync) could move to Arc-per-entry; boot-time warm-open of
recent chats (PARITY gap) is unchanged.

## 8. Acceptance

- Viewer laptop after browsing 20 chats incl. images: **<250MB** (from ~600).
- RSS flat ±10% over 8h mixed use (no monotonic ratchet).
- mem-smoke thresholds in CI: engine idle <40MB; stream-retention <3× raw
  text; reopen p95 <100ms; idle creep <1MB/10min.
- No feel-budget regression (§2), verified per landing PR.

## 9. Crew 0.1.67 performance audit (2026-09-05)

Audit baseline: `ba0511d` on `origin/main`. This pass covers desktop transcript
rendering, engine/room backpressure and document lifetime, the edge's session
projection/storage path, and iOS rendering/reconnection/persistence.

### Root causes and fixes

- Desktop animation frames rebuilt the message rail from the whole transcript
  and searched all rows for every prompt. The rail is now revision-cached, with
  one row-index map per revision and logarithmic active-tick lookup.
- Visible markdown rows scanned all blocks just to request one block's
  highlights. Highlight lookup now receives that block directly. Render-cache
  invalidation removes a row bucket rather than scanning every settled entry;
  code-copy closures share the cached source instead of copying it each frame.
- Incremental markdown discarded its tail with a full-prefix retain scan and
  copied append deltas. Ordered truncation and borrowed suffixes remove those
  costs. Structural row retirement now prunes stale entry, parse, and highlight
  caches while retaining explicitly loaded history.
- PTY raw output, subscriber queues, and time-only batches were unbounded.
  Raw queues, event queues, and batch sizes are now bounded, with lossless
  per-terminal backpressure and cancellation on close. Data-free filesystem
  invalidation kicks coalesce to one pending notification.
- Linux file-access notifications could make Git diff capture trigger another
  capture of its own reads. Read-only access events no longer request capture;
  close-after-write and mutations still do.
- Execution-key aliases retained chat documents after canonical eviction/purge
  and duplicated snapshot/status work. Eviction removes aliases; sweeps visit
  canonical document handles once.
- Slow room sends and missing ACKs retained unlimited callback updates/batches.
  Queue overflow now requests the existing reconnect/VV recovery path. Limits
  are update-count bounds, not an absolute cap on a single snapshot's bytes.
- Edge fragment reservations lacked aggregate limits and duplicate-index
  handling. Per-socket reservations now enforce the existing sync limits and
  validate complete, exact-size reassembly. Blob size/existence reads use SQL
  `SUM(length(bytes))`, avoiding payload materialization.
- Edge tail projection walked unrelated document roots, and continuation
  joining repeatedly copied all accumulated parts. Projection is now scoped to
  messages/metadata and joining appends into one owned output array.
- iOS numeric highlighting could loop forever on numerals outside its
  continuation predicate. It now always advances. Token recoloring walks
  character indices monotonically rather than rebuilding every prefix.
- iOS reconnect callbacks could schedule duplicate sockets; obsolete batch IDs
  retained payloads after reconnect. Generation-checked redial is single-flight
  and resubmission derives missing operations from the durable document VV.
- iOS snapshot debounce created a sleeper for every update, and pruning
  protected the obsolete `ws3_` prefix. Each saver now keeps one sleeper and
  pruning preserves current `ws4_` workspace snapshots. Retired mobile parse
  caches are pruned by live part keys.

### Measured evidence

Local optimized microbenchmarks, best of five; identical old/new Loro outputs
were asserted. These measure the named operations, not end-to-end latency.

| Scenario | Baseline | Fixed |
| --- | ---: | ---: |
| Join 1,000 continuations | 4.565 ms | 0.107 ms |
| Join 4,000 continuations | 28.423 ms | 0.135 ms |
| Join 10,000 continuations | 258.059 ms | 0.657 ms |
| Tail with unrelated 8 MB root | 2.566 ms | 0.023 ms |
| iOS Unicode numeral highlighting | Exceeded 1.5 s deadline | Three fixtures in 11.1 ms |

Actual SQLite smoke covered missing, empty, multi-chunk 4.5 MB, and deleted
blobs. The iOS lifecycle smoke used the real Swift Loro client against a local
WebSocket endpoint: eight evictions with lost ACKs produced one socket at a
time, full durable convergence, and no redial after stop. Trailing persistence,
immediate flush, absence of a delayed duplicate save, and workspace retention
beside 81 newer snapshots passed.

The optimized iOS simulator benchmark built 5,000 transcript rows in 45.06 ms
cold and 2.07 ms warm, with 24.80 ms off-main entry decode. The 120-turn demo
rendered the streaming reply, allowed history scrolling, and returned to the
tail through its jump control. These are current-path measurements, not a
before/after claim for this release.

Release validation passed 707 Rust library tests, 17 terminal/diff integration
tests, 93 Edge tests, and 18 release-workflow/runtime-guard tests. The real local
Edge/Rust collaboration smoke covered authenticated relay, reconnect, forged-
actor rejection, and revocation. A real PTY firehose delivered all 64 MiB in
order after a stalled subscriber resumed; another terminal stayed responsive,
and closing the stalled terminal took 154 microseconds. The candidate desktop
app passed composer-send, rendered-reply/code, and history-scroll smoke checks.
Production and staging iOS simulator builds both rendered the large transcript.

Typechecking was intentionally not run: global agent instructions prohibit it.
The normal `main` deployment workflow retains its Typecheck step. This release
uses a temporary deployment branch omitting that step, while retaining Rust
build, real collaboration smoke, Edge tests, and immutable-candidate checks.

### Staging workspace persistence repair

The subsequent official desktop cutover exposed a separate device-discovery
failure: the device relay was connected, but workspace requests returned HTTP
500 with `SQLITE_TOOBIG`. Full workspace backfills could exceed SQLite's
approximately 2 MB value limit before the normal post-insert compaction ran.
Oversized pending updates now persist the merged document through the existing
chunked snapshot store. Smaller updates retain the incremental log path; no
workspace reset or history deletion is required.

The regression failed with the original SQLite error before the fix. All 94
Edge tests then passed. A real local Worker persisted a 2,100,000-byte payload
and recovered identical bytes after a process restart. The installed official
desktop binary also passed authenticated collaboration, reconnect, forged-actor
rejection, and revocation smoke checks.

The staging-only deployment is Worker version
`d49376e2-3ad0-42cb-899f-ae25633e9cb4`, from source commit `d658daf`.
A fresh authenticated cloud client verified the preserved desktop device ID,
version 0.1.67, all 27 expected unarchived sessions, and a 3.9-second-old
presence heartbeat. Workspace stats returned HTTP 200 with a 692,129-byte
snapshot and 13 ms cold replay. Physical-phone UI visibility was not inspected.

### Corrective staging release: desktop 0.1.68 and iOS 1.0 (3)

Oversized persistence now also preserves writes accepted while snapshot storage
is awaiting completion. Compaction folds the document before exporting and
retains force-trimming behavior. The concurrency regression failed before the
fix; all 95 Edge tests passed afterward. A real Worker compacted a 2.1 MB
historical overwrite to 323 bytes and recovered it after a process restart.
Staging Worker version `1fd8be1e-7f6f-45ee-927d-344249fa96f5` uses source
`ad95509`; production was not changed by this corrective deployment.

The blank mobile transcript was a routing error, not a rendering failure.
iOS supplied the project scope as a fallback deployment ID, opening a separate
empty room for an ordinary desktop session. The affected session had 46
messages in its correct room and none in the fallback namespace. AppConfig now
adds a deployment ID only when one is explicitly supplied, preserving actual
Scaffold deployment scopes.

Both desktop and iOS hide spaces without unarchived sessions. Obsolete desktop
pinning exceptions were removed. The user's authorized cleanup consolidated 20
duplicate spaces across seven paths, moving 52 archived sessions before deleting
their empty source spaces. All 699 stored session rows and all 27 unarchived
sessions were preserved. The two occupied spaces contain 24 and three
unarchived sessions; unused unique spaces remain stored but hidden.

Native verification rendered the affected session's actual 46-message snapshot
in an isolated iOS simulator backed by a local Worker. Both the native iOS
sidebar and the desktop viewport attached to the live engine showed only
`ashler-platform` and `ashler-comet`. All 525 Rust UI library tests passed;
the desktop executable, staging simulator app, and staging iOS archive built
successfully. TestFlight accepted staging 1.0 (3) for the existing internal
tester group. Physical-phone installation and rendering were not inspected.
Typechecking was intentionally not run because global instructions prohibit it.

Official desktop release run
[`33988484041`](https://github.com/Ashler-AI/comet/actions/runs/33988484041)
successfully published staging 0.1.68 from `15cf6d7`. Both artifact checksums
and the app signature verified; the installed executable matched the official
artifact byte-for-byte. Its native viewport rendered the live transcript and
only the two occupied spaces. The obsolete window and debug viewport were
closed, while the existing 0.1.67 engine deliberately remained running so the
active agent turn was not interrupted. No new workspace reconnects occurred
during the UI cutover.

The simulator retained readable transcript content across a local Worker
process restart, with both workspace and affected-session sockets rejoining.
The isolated app installation, Worker state, and throwaway probe were removed
after verification. Rollback bundles and the pre-cleanup workspace snapshot
remain under `~/Library/Application Support/Crew Migration Backups/`.

#### Archived-history availability and authorization follow-up

The 699 count refers to workspace session records, not a guarantee that every
historical transcript is cached locally. Per-user membership filtering excludes
117 archived records from the 582-row view. In both the live principal-scoped
DocsStore and the pre-cutover backup, 24 of those records have nonempty decoded
transcripts, 17 have valid snapshots without messages, and 76 have no snapshot.
All 41 existing snapshots are byte-identical to their pre-cutover copies; none
is backup-only. No matching journals were found in the principal-scoped or
legacy staging project directories, and none of the 41 snapshots contains an
ownership publication. Cloud transcript availability for these historical
records was not verified. Unarchiving alone does not create the missing
per-user membership; recovery must use the existing authorized session-ID or
invitation flow rather than bypassing the ownership boundary.

Follow-up review found an authorization-ordering race: document materialization
could yield after the socket's final authority check. Update and join handlers
now materialize first, authorize last, reject closed/reset sockets, and use only
the current live document after that await. This also avoids retaining a
document freed by concurrent compaction. The compaction regression now asserts
that its persistence barrier was actually entered instead of allowing a
sequential run to pass.

The revocation regression failed against the pre-fix implementation. All 102
Edge tests passed with the final guards, including trim, idle release, and
reset/rematerialization interleavings. The real local Worker/official-desktop
collaboration smoke also passed authentication, relay, reconnect, forged-actor
rejection, and active revocation.

The final guard deployment is staging Worker
`52fd8306-d242-4639-84ac-e29322fdfc26`, from `e83a5d7`. The installed official
desktop's live sync command confirmed the workspace and open chat rooms
connected after rollout, with zero full resyncs. The existing engine process
was not restarted. The recovery audit also confirmed that all 610 pre-cutover
DocsStore snapshot IDs remain present in the live store.

### Mobile command and desktop latency corrections

The phone's ordinary-session sender wrote command-ledger entries directly,
without the host-local admission record required to drain them. Commands now
go through `QueueCommand` on the actual host; Scaffold commands retain their
verified controller route. Admission/transport failures remove only the
matching optimistic send, expose the error, and restore its draft.

Native verification also caught a creation-ordering defect: directly writing
a chat row did not establish the host's principal-scoped session membership.
Creation now awaits the host's authenticated `Mutate:createChat` before local
echo or sending. No ownership check was relaxed. A real iOS simulator, local
Worker, and Rust mock host completed both the initial command and a second
command to that existing session, producing four transcript entries.

The desktop's eager sidebar layout constructed and shaped offscreen rows on
every redraw. Rows now retain their existing geometry and scroll container
while constructing content only when intersecting the clip. Native session
selection and history scrolling rendered correctly. Cold document reads are
offloaded from the RPC dispatch loop, and command ancestry/duplicate lookup
no longer materializes the entire transcript.

A throwaway dev-profile smoke with 500 messages / approximately 4 MiB verified
raw continuation ancestry and torn-row duplicate semantics. Fifty old-style
full-transcript ancestry lookups took 261.301 ms; fifty narrow lookups took
216.375 microseconds. These are document-operation timings, not end-to-end
send or session-open latency. The changed Rust library suites passed 741 tests.
The actual Swift room client also received fresh staging status updates;
an initial transient stale interval was not treated as proof of a persistent
decoder or delivery defect. Unsupported timestamp compatibility code was
removed rather than retained as a speculative fix.

The final native two-turn smoke completed with four transcript entries.
During a separately paced live turn, the phone rendered streaming content and
`Riffing... 1s`; its session list showed an explicit `Running` label while a
desktop-submitted command streamed. The desktop composer cleared and the
captured response showed `Pondering... 1s`. These are observed UI states, not
latency percentiles.

Official release run
[`33996352949`](https://github.com/Ashler-AI/comet/actions/runs/33996352949)
published staging desktop **0.1.69** from `fe64931`. Artifact checksums and the
installed executable matched; the app's ad-hoc signature verified. The
repository's unnotarized packaging fails Gatekeeper assessment. Installation
through the existing internal staging launcher received explicit user approval;
no system-wide trust setting was changed.

The installed viewport rendered populated session history and navigation.
The old engine unexpectedly exited during the separately targeted viewport
shutdown; this was reported as a process-isolation anomaly. The new embedded
0.1.69 engine recovered two journals: one resumed and reported `working`,
while the other reported `errored` because its original OMP writer remained
active. The user explicitly chose to leave that writer running. This cutover
is **not** claimed to have been uninterrupted.

Staging iOS **1.0 (4)** archived successfully and App Store Connect accepted
its cloud-signed upload. After web authentication became available, App Store
Connect showed all four builds in the internal group and confirmed that its
tester had installed **1.0 (4)**. No additional invitation was needed. Physical
phone interactions were not directly inspected.

Owned simulator, mock-host, Worker, copied-source, and profiling fixtures were
removed. Final archives, rollback bundles, UI screenshots, the two-turn log,
and recovery receipt are retained in the private `latency-0.1.69` directory
under `~/Library/Application Support/Crew Migration Backups/20260905-staging/`.

Typechecking was intentionally not run because global instructions prohibit it.

### Sidebar sizing and trailing controls follow-up

Deferred card layout exposed an implicit parent-stretch assumption: the
rendered card root did not request the full available width. Active cards
and the settled opacity wrapper now explicitly fill their fixed-height slots.
The existing clip-aware construction and scroll geometry remain intact.

Status indicators and settle actions share a trailing slot with an 18px
minimum width. The hover overlay no longer crowds a long folder label.
In the native desktop smoke, short and long cards had matching dimensions;
clicking the relocated settle button moved only its fixture into Settled,
preserved the selected Short session, and retained full-width settled rows.

A freshly built native iOS simulator showed consistent rows and the live
`Running` label. Its initial and existing-session commands both completed,
producing four transcript entries; desktop settlement also removed that
fixture from the phone's active list. The mock host deliberately paced its
output, so these runs are behavior checks rather than latency measurements.
No iOS source change is required for this desktop-only layout correction.

Official release run
[`34005890849`](https://github.com/Ashler-AI/comet/actions/runs/34005890849)
published staging desktop **0.1.70** from `0b1abdf`. Both artifact checksums,
the installed executable comparison, and the ad-hoc signature verified.
The user approved the internal build's installation. The new viewport
connected to the already-running 0.1.69 engine; its old window was minimized,
not closed. The engine retained the same PID and device identity throughout
this handover. The surviving OMP writer was not taken over.

The actual installed viewport rendered equal-width cards and the trailing
status rail. Official artifacts, screenshots, the TestFlight installation
receipt, and the native two-turn log are retained in the private
`sidebar-0.1.70` directory beside the earlier release evidence.

### Limits of this audit

A three-second sample of the already-running desktop process showed a 1.2 GB
physical footprint (2.1 GB peak); `vmmap` attributed 944.5 MB of swapped memory
to IOAccelerator regions. That GPU high-water observation is **not attributed
to or claimed fixed by this pass**. The older process was an active session,
not a controlled idle baseline. The isolated two-turn candidate smoke reported
60.5 MB Metal device allocation, a 1 MB atlas, and one 2 MB instance-pool buffer.
That small workload is not comparable to the user's active session. The
eight-hour residency acceptance criteria above remain unmeasured.

Source-visible costs still needing workload attribution include whole-live-
reply display-tree construction, very large individual code blocks, Mermaid
layout, and collaboration/command-ledger projection. No arbitrary eviction or
notification-cadence change was introduced to hide those costs.
