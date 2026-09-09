# Native Crew worker contract

## Delivery and verification boundary

This change adds Crew-side source APIs; it does **not** add a Firstmate backend.
The new endpoints and Rust regressions have **not been compiled or executed**.
Local Cargo build/test/typechecks are prohibited for this task. Authorized remote
verification is blocked by the uncertain accepted handoff recorded below. Do not
install this branch over the running app or treat a source review as runtime proof.

The assigned branch started at `82639e3eb9691da9bddce08c2879eb912686df0b`
(workspace version 0.1.63). The installed app reports 0.1.85. The authoritative
current upstream reference is `origin/main` at
`c0195945b6a7dcce0c3319ffa1e00157c939afbe` (0.1.86), including installed OMP
historical attachment capture from `<OMP root>/blobs`. The earlier inspected
`99710db28eb3d71342a37e03dcbea3c4d66176d5` release record identifies
`e771970915b4b10565a825dca6140e13703b682f` as the 0.1.85 staging source and
[release run 34387642609](https://github.com/Ashler-AI/comet/actions/runs/34387642609).
Native session handoff entered in `dcbc5bc5a682af544c985bdd0d16bd28467f761e`
(0.1.84); 0.1.85 corrects source-preserving handoff. The older starting base
lacked that CLI variant. The parent subsequently authorized reconciliation in
this isolated copy: checkpoint `e983dad5281a582448cd2b2a8640fa870db6ee9f`
was replayed onto verified canonical `origin/main` (`99710db`) as
`b7d299aadd0e4645f89fafea71237ea8b1517127`, whose direct parent is that main.
Only this implementation commit was replayed; an initial plain-rebase attempt
that touched unrelated historical UI work was aborted without resolving it.

Bounded conflicts were README additions and CLI imports/variants/dispatch.
Both upstream handoff and worker paths were retained. Upstream native handoff
preparation/tests, source worktree transfer, OMP integration, inference relay and
auth files compare byte-identically with the new base. This resolves the source
divergence blocker, **not** runtime compatibility verification. The installed app,
running services and credentials remain unchanged. No installation is authorized.
`fix/native-handoff-source-repo` resolves to the same inspected `99710db` commit
as `origin/main`; it is not an additional branch that must be merged. The
`e771970` release commit is an ancestor of that ref. Also coordinate additive transcript
provenance work in `doc_host.rs`; peer command/message IDs remain unchanged here.

The combined visibility-first checkpoint is
`a77ba6e508a67c77e864ce4d8146041450ee1ae7`, containing visibility final
`3764beeb7457aab75cdad28e8235acf9dc039250` followed by worker integration.
Non-rewriting merge `af4f0261e737445791a29b049cb570b41eb0eca9` retains that
checkpoint as its first parent and `c019594` as its second parent. Both upstream
historical attachment changes and the existing native handoff/auth flow are
retained; no active worktree or running service was replaced.

## Boundary and transport

Crew executes sessions. Firstmate owns briefs, dispatch profiles, task success,
landing, quota policy, supervision cadence and teardown authorization.
No Herdr markers, tmux fallback, terminal capture, desktop permissions or separate
control service are involved. Use the Crew-supplied `COMET_EXECUTABLE` and
`COMET_IPC_PORT`; preserve `COMET_SESSION_ID` as the actual owning chat ID.
Never guess an executable/session ID or substitute a different provider.

CLI commands connect to the existing loopback WebSocket RPC endpoint. RPC uses
one JSON envelope per WebSocket text message:

```json
{"id":1,"method":"ReadWorkerSession","params":{"chatId":"<worker UUID>","ownerChatId":"<owner UUID>"}}
```

Unary success is `{ "id": 1, "ok": ... }`; failure is `{ "id": 1, "err": ... }`.
Watch methods emit `{ "id": 1, "item": ... }`; cancel with
`{ "id": 1, "cancel": true }`. A disconnected transport is **unreadable**, not
proof of missing, idle, completed or failed work.

New worker methods require `LocalController`, a canonical UUID pair, an existing
locally hosted owner chat and the exact persisted owner/worker binding. They are
not remote-device forwarding APIs. UUID binding prevents accidental cross-task
control; it is not a secret capability or a new security boundary against other
programs already authorized to use the same local Crew control endpoint.
Existing Crew authority/agent-route handling remains responsible for execution.

## Provision exactly one idle worker

Persist a fresh worker UUID and the entire request **before** the first call.
Reuse them verbatim after timeout/disconnect. The project is a canonical absolute
Git checkout root; the base ref, title, model and effort are mandatory.

```sh
"$COMET_EXECUTABLE" session worker ensure \
  --session "$WORKER_ID" --owner "$OWNER_ID" \
  --project "$PROJECT_ROOT" --base main --title "fm-task-label" \
  --model openai-codex/gpt-6-astra --effort xhigh
```

RPC `EnsureWorkerSession`:

```json
{
  "chatId":"<persisted worker UUID>",
  "ownerChatId":"<actual owner UUID>",
  "projectPath":"/canonical/project/root",
  "baseRef":"main",
  "title":"fm-task-label",
  "model":"openai-codex/gpt-6-astra",
  "effort":"xhigh"
}
```

`--owner` may use the actual ambient `COMET_SESSION_ID`; adapters should pass the
recorded owner explicitly. Effort is a serialized native `ReasoningLevel`; the
engine validates the exact model and supported reasoning level against the
existing OMP catalog. No model/effort fallback. A catalog match is **not** proof
of authentication, funded quota or successful inference. `quota-axi` availability
and quota-aware dispatch are outside this implementation.

The worker uses OMP, workspace-write sandbox, empty model options and Crew's
automatic existing account route (`agentAccountId: null`), not copied credentials.
Ensure does not send a prompt or launch inference. Firstmate can choose supported
model/effort values, including its separately configured low/medium/xhigh dispatch
profiles; no Firstmate profile policy is encoded in Crew.

The engine reserves a durable `workerBindings` row before filesystem work,
creates/reuses the normal visible project space and chat, materializes
`<managed-worktrees>/<repo-name>/worker-<worker UUID>` on a fresh
`comet/worker-<worker UUID>` branch, and binds `cwd`, `checkoutId`, and branch to
that exact chat. No runnable primary-checkout fallback during provisioning.
Success persists workspace state before acknowledgement. The filesystem operation
continues when the initiating connection disappears.

Retry returns the same binding. Reusing the UUID with another owner, project,
base, title or initial configuration refuses. Existing ordinary chats cannot be
adopted as workers. A recovered checkout must be a registered linked checkout of
the exact repository, with matching path and Git backlink/checkout identity.
Dirty files, untracked files, commits and branch renames are retained. A missing
path with an existing branch or Git worktree registration is ambiguous and
refuses; no reset, force-add, prune or deletion is attempted.

The same synchronous identity guard runs before ordinary command admission and
native execution, not only ensure/status/recover. It reads the current `.git`,
`commondir` and `gitdir` backlink, requires the exact linked Git directory under
the project's common directory, and recomputes Crew's existing device-scoped
checkout ID. It rejects redirected/symlinked/moved metadata, a sibling or foreign
checkout, and changed local owner/device/configuration bindings without queuing
or starting a turn. Branch renames and working-tree changes do not change identity.
Git's own `rev-parse` must also report the exact root, Git directory and common
directory. This honors effective Git configuration instead of assuming pointer
files capture `core.worktree` semantics. Readback failure refuses admission.

Initial configuration is immutable for this worker identity. Generic config/cwd
changes are detected and execution fails closed rather than silently running at a
different model, effort or project. `binding.config` is the requested contract;
`chat.config` is the persisted execution row. Do not use `Mutate setChatConfig`
to change an owned worker. A separately authorized new task may use a new UUID.

## Messaging and durable receipts

Use the existing messaging operations, not a second delivery protocol:

```sh
"$COMET_EXECUTABLE" session send "$WORKER_ID" "$INSTRUCTIONS" \
  --from "$OWNER_ID" --command-id "$PERSISTED_COMMAND_ID"
"$COMET_EXECUTABLE" session worker status --session "$WORKER_ID" --owner "$OWNER_ID"
```

`SendPeerMessage` already accepts `commandId`; the CLI now exposes it. Persist it
before send and retry the same ID and payload after uncertain delivery. The
same ID with different peer text/provenance refuses. Initial sends to an owned
worker must name its bound owner. The receipt `{commandId, threadId}` means
**durable admission**, not execution, reply, terminal completion or task success.
Command admission and resolution snapshots bypass the background debounce.

Workers reply with the existing exact command correlation:

```sh
"$COMET_EXECUTABLE" session reply --session "$WORKER_ID" \
  --command "$RECEIVED_COMMAND_ID" "$REPLY"
```

`ReplyPeerMessage` derives `reply:<commandId>` and preserves `threadId`,
`sourceChatId`, `replyTo` and hop count. Peer command IDs remain transcript user
message IDs. `send --wait`, `reply --wait` and `session wait` register a **live**
waiter; they do not retroactively consume a reply after reconnect. A missed live
wait is not permission to resend under a new ID. Inspect durable `replies` in the
worker snapshot instead; each is a typed owner-ledger command from this exact
worker, including `payload.text`, `replyTo`, `threadId`, status and resolution.
Replies may be rejected for execution at an idle owner without run configuration
but still remain durably readable. A reply saying “done” is not a terminal event.

A worker can own a child worker. The child's reply is a narrow exception to the
initial-send owner restriction, not general non-owner send authority. Admission
and execution require the current same-device parent/child binding, a ready
child, and the exact durable original peer command in that child. Its source
must be this parent, thread must match, hop must increment within the limit, and
the reply ID must be `reply:<original command ID>`. A device-local immutable
fingerprint binds the original command and worker identity; synced command IDs
or copied owner strings alone cannot authorize a reply. That fingerprint remains
after `Applied`, enabling later replies after reconnect. Rejected/cancelled
exchanges do not authorize new child replies. The internal
`worker-peer-authority/v1/` command-ID prefix is reserved; adapters must not use it.

The existing command ledger marks processed before side effects: a crash in that
window can leave uncertain delivery. The snapshot reports `unknown`, not success
or permission for automatic replay. Exactly-once external side effects are not
promised. Keep the same worker identity, inspect commands/journal/worktree, and
obtain task-policy authorization before issuing a new instruction ID.

## Authoritative observation

`ReadWorkerSession` / `session worker status` returns:

- `version: 1`.
- `binding`: immutable identity/configuration, retained worktree and persistent
  `paused` / `closed` lifecycle fences.
- `chat`: normal Crew chat row, including `spaceId`, `cwd`, `checkoutId`, current
  branch, configuration and harness session identity; nullable when removed by
  an enclosing workspace operation.
- `state`: engine-derived lifecycle state, never a transcript regex or unread dot.
- `latestEvent`: nullable `{seq, event}` from the native durable run journal.
- `commands`: exact worker command ledger with admission/execution resolutions.
- `replies`: exact correlated peer replies durably recorded in the owner ledger.

State precedence:

| State | Meaning |
|---|---|
| `busy` | Live turn or dispatch preparation; an earlier Done is not current completion. |
| `waiting` | Live native run in `AwaitingInput`; approval/input must follow existing native path. |
| `closed` | No active turn and persistent closed fence; workspace retained. |
| `interrupted` | Paused fence, native interrupted Done, or journal activity without terminal evidence and no live turn. |
| `missing` | Binding exists but chat row is absent; never silently recreate or retarget. |
| `provisioning` | Reserved identity not yet bound to a checkout; retry identical ensure. |
| `unknown` | Processed command has no durable resolved outcome and no live turn. |
| `queued` | Unprocessed pending command; not idle/completed. |
| `failed` | Latest command rejected/expired, or native `Done.status = errored`. |
| `completed` | No active/pending/uncertain work, latest native event is `Done.status = completed`. |
| `idle` | No native event yet and no pending work. |

A close/interrupt timeout may leave a persisted fence while the snapshot still
reports busy/waiting. Retry the same lifecycle control; do not declare stopped
until successful settlement. `completed` is the latest **agent turn** outcome,
not proof that the assigned task passed tests, committed, landed or met policy.
Consumers must inspect their command resolution and compare journal sequence to
the pre-send baseline. No per-task success flag is inferred from a human reply.

`WatchChats` provides normal visible chat metadata, not authoritative terminal
outcomes. `WatchSessions` is the existing workspace status projection with
staleness semantics; do not reinterpret a stale/missing row as completed.
`WatchDocMessages` is for content: first `{reset:[...]}`, then delta frames with
`upsert`, `append`, `remove`, `count`, optional `before`. Reconnect requests a new
reset; `ReadDocMessages` pages older transcript content using `before`.

## Safe lifecycle

```sh
"$COMET_EXECUTABLE" session worker interrupt --session "$WORKER_ID" --owner "$OWNER_ID"
"$COMET_EXECUTABLE" session worker recover   --session "$WORKER_ID" --owner "$OWNER_ID"
"$COMET_EXECUTABLE" session worker close     --session "$WORKER_ID" --owner "$OWNER_ID"
```

RPC `ControlWorkerSession` accepts `{chatId,ownerChatId,action}` where action is
`interrupt`, `recover` or `close`.

- Interrupt installs a persisted pause fence, cancels a route preparation,
  serializes with dispatch, waits for native interrupt settlement and cancels
  remaining pending commands. Resume requires explicit recover.
- Close additionally sets the persistent closed fence and archives the chat.
  It retains checkout, branch, unlanded work, transcript, journal and binding.
  It does not stage background worktree deletion. Worker bindings also protect
  the retained checkout from ordinary staged cleanup of a shared/forked chat.
- Recover validates the same existing checkout/configuration, cancels any
  leftover pending commands from a stopped lifecycle, clears fences and
  unarchives the same chat. It does **not** create another worker, send a prompt,
  automatically retry a failed instruction or take over an unrelated process.
  Native session continuity on the next explicit message uses existing Crew
  resume rules. Engine crash recovery keeps its existing bounded resume policy;
  worker dispatch fences are checked there too.
  An interrupted/closed reservation without a bound checkout can recover to
  `provisioning`; this only clears the lifecycle fence. It remains unrunnable
  until identical ensure validates/reuses any retained partial checkout and
  finishes the original binding. Recovery never resets or deletes that checkout.
- These operations never merge, commit, discard, delete or land code. Firstmate
  must make its own explicit landing/cleanup decision. There is no worker API
  for deleting retained work. Do not compose generic delete calls as “close”.
- Invalid/missing owner, changed host/project/checkout/configuration, unknown
  worker, unsupported model/effort or transport failure is a hard refusal; no
  automatic provider/backend switch or takeover.

## Mapping to existing native methods

| Existing method | Reuse / boundary |
|---|---|
| `Mutate createChat` | Same `WorkspaceHost::create_chat` and normal visible chat/space rows; worker wrapper adds durable ownership and provisioning fence. |
| `Mutate setChatConfig` | Generic sessions retain full-config replace; owned workers pin their initial explicit OMP config and refuse mismatched execution. |
| `CreateWorktree {repoPath,branch,chatId}` | Existing server-owned bind path is reused through `bind_chat_worktree`. Generic creation allocates a random new path every call and is **not** retry-idempotent. Worker wrapper uses the same Repos/Git machinery with a deterministic path and recovery checks; no adapter blind retry of generic CreateWorktree. |
| `QueueCommand` | Existing ledger/executor, typed `Run`/`Steer`/`Queue`/`RespondInput`/`Interrupt`; native worker guards validate pinned execution config. Peer send/reply also use this ledger. Do not interpret `Applied` as turn completion. |
| `WatchDocMessages`, `ReadDocMessages`, `WatchChats` | Existing content and visible workspace projections, unchanged wire shapes. Status is read separately from native engine/journal. |
| `CancelChatStartup` | Ordinary first-send rollback only; may delete a just-created checkout and refuses after admission. Owned workers explicitly reject it: use retention-only close even after partial provisioning. |
| `ForkSession` | Existing context fork, not isolated worker provisioning and not an idempotent recover operation. It does not establish Firstmate ownership and can share the source checkout. Do not use it to recover a worker. |
| `TakeOverOmpSession` | Existing explicit verified writer takeover, not automatic worker recovery. Provider/ownership approval boundaries remain intact. |
| `Mutate setChatArchived` / `deleteChat`, `DeleteWorktree` | Not safe Firstmate teardown primitives. Worker deleteChat is rejected; worker archive never stages deletion. Direct filesystem deletion remains outside worker lifecycle scope. |
| Native Crew-to-Scaffold handoff | Upstream landed CLI/preparation/transfer are preserved after reconciliation. No worker call invokes it, changes its payload, replaces its auth path, or synthesizes provider markers. Behavioral compatibility remains unverified until authorized regression execution. |

## Required Firstmate cutover (separate repository)

No active Firstmate copy was edited. Its existing registry accepts only
`tmux herdr zellij orca cmux`; missing recognized markers currently falls back to
tmux. End-to-end Firstmate integration therefore requires coordinated tracked
changes, not only installing Crew:

1. Add an explicitly selected `crew` adapter and register it in known/spawn sets,
   source dispatch, required-tool checks and explicit capability/version refusal.
   Do not claim auto-detection before verifying real native markers; never fake
   `HERDR_ENV` or accept a silent tmux fallback.
2. Use native worker provisioning, not terminal/harness shell launch. Crew owns
   the isolated checkout; do not also allocate a Treehouse worktree for it.
3. Persist the exact worker UUID, owner UUID, project root, returned space/device,
   checkout path/ID, initial configuration, request spec, instruction IDs and
   thread IDs in task metadata before dispatch. Labels are display only.
4. Map send/read/watch/reconnect onto typed native command/reply/status data.
   Implement queued/unknown/unreadable states fail-closed; no composer Enter
   simulation or “done” transcript parsing.
5. Extend metadata validation and task-scoped teardown guards to compare that
   exact binding before interrupt/close. Do not use generic delete/archive as
   teardown, and do not enumerate-and-close all Crew sessions.
6. Supervision, secondmate support and away-mode supervisor injection require
   explicit adapter contracts/tests; existing tmux/Herdr-only assumptions do not
   become valid merely because the worker is visible in Crew.

## Verification handoff

### Exercised here: installed 0.1.85 only

- `/Applications/Crew.app/Contents/MacOS/comet session current` returned the
  assigned exact session ID.
- `.../comet session --help` listed existing read/fork/handoff/send/reply/wait;
  it has no worker subcommand.
- Native session reply/send returned durable command/thread receipts to parent.
- Read-only WebSocket calls at the supplied IPC port: `LocalDevice`,
  `WatchChats`, `WatchSessions`, `WatchDocMessages` succeeded. The exact assigned
  chat projected its isolated cwd, `openai-codex/gpt-6-astra`, `xhigh` and working
  state; transcript frame was `{reset: ...}`.
- `ReadWorkerSession` on the installed app returned
  `unknown method: ReadWorkerSession`. This is **not** validation of new code.

No local Cargo builds/tests/typechecks, lint, formatters, project-wide suites,
deployment, service replacement or Git push/PR/merge to the upstream repository
ran for this combined source. No new worker endpoint has been exercised.

### Combined-source verification blocked

The authorized single native verification handoff used source checkpoint
`a77ba6e508a67c77e864ce4d8146041450ee1ae7`, a saved pre-handoff source-config
readback of `openai-codex/gpt-6-astra` at `medium`, and database environment `local`.
Source configuration was restored and read back as `xhigh` after the attempt.
The upload request failed after the native controller accepted these identities:

- Sandbox: `rcs_2fb634f12de0472c2181609a`.
- Native session: `682cbfda-05ee-489e-90c0-18d06d38ba93`.
- Upload: `rcup_044d4f1a-ecc9-4a03-a700-f50adb1e37cc`.

Bounded native inspection reported `ready`, lifecycle epoch 1, database `local`,
and retained the exact session reference/grant. The exact room returned an empty
transcript and session list; that is **not** proof that no remote side effects
occurred. Missing/partial/completed upload and initial-command admission versus
lost receipt remain unknown. Initial remote model/effort execution is unverified.
`Inspect` exposes no upload state; exact-scope `PrepareScaffoldSession` does not
recover final chat creation plus initial command admission. Re-running CLI
`session handoff` allocates another identity, not an idempotent retry. Recovery
inspection stopped at the parent's instruction: no Prepare recovery, retry,
fallback, new command or sandbox deletion was performed. Operator inspection of
the preserved accepted identity is required before any further remote work.

The required local OpenCode read-only review of combined `a77ba6e` against its
confirmed `origin/main` merge base `99710db` found two worker defects: current Git
identity was not checked at ordinary execution admission, and the owner-only
peer guard rejected legitimate child replies. It reported no additional original
peer-message visibility findings. The review ran no tests/builds or handoff
operations. Corrections require a further OpenCode review and authorized native
behavioral verification; the preliminary review is not PR readiness.

A subsequent static OpenCode review against `c019594` confirmed the child-reply
correction but found effective Git root overrides and a stopped-provisioning
recovery dead end. Both have source corrections and prepared regressions. A
disposable Git-only experiment confirmed that direct `config.worktree`
`core.worktree` redirects the root while leaving the Git-directory identity
unchanged, and that the exact effective-root query detects the redirect. This
did not compile or execute Crew. Included configuration did not redirect the
effective root in that experiment; no rejected-include claim is made. A final
unchanging source checkpoint must undergo OpenCode review before publication.

### Commands for an authorized remote verification environment

Use the reconciled source in a separate disposable checkout/data directory
with stable Rust toolchain, Git, C compiler/linker and Cargo dependency access.
The focused engine fixtures use a deterministic in-process test harness and
local temporary repositories; they need no OMP credentials, quota or desktop.

```sh
export ASHLER_INCREMENTAL_TSC_CHECKS=false
cargo test -p comet-engine --test worker_sessions -- --nocapture
cargo test -p comet-engine --lib repos::tests::worker_ -- --nocapture
cargo test -p comet-engine --test peer_messages -- --nocapture
cargo test -p comet-engine --test restart_resume -- --nocapture
cargo test -p comet --bin comet session_parser_tests -- --nocapture
cargo test -p comet --bin comet session_cli::tests -- --nocapture
cargo test -p comet-engine --lib rpc::scaffold_session::tests -- --nocapture
cargo test -p comet-engine --lib worktree_handoff::tests -- --nocapture
```

Also run upstream's native Scaffold session/handoff focused regressions from
`crates/engine/src/rpc/scaffold_session_tests.rs`, plus source-preserving worktree
handoff tests, after resolving the overlapping engine/CLI changes. Preserve all
upstream checks; the old-base test list is not a substitute for handoff coverage.
The reconciled 0.1.86 source also requires the existing
`cargo test --locked -p comet-engine --lib historical_attachments` lane, including
the installed-layout capture test; no handoff upload retry is part of that test.

New regressions defend:

- repeated ensure/restart preserves one checkout, config and owner; different
  owner/colliding identity refuses;
- existing commits, edits, untracked files and renamed branches survive recovery;
- orphan branch, unrelated repository checkout and symlink escape refuse;
- ordinary send and direct native dispatch reject sibling/foreign `.git`, moved
  backlinks/Git directories, altered common directories and symlinked metadata;
  restoring original metadata retains renamed branches, commits, dirty and
  untracked files; changed owner-device bindings reject before harness admission;
- transcript text claiming completion while live stays busy; native errored vs
  completed Done remains distinct;
- close during a live run retains unlanded files, fences new sends, and recovery
  does not replay instructions;
- message ID retry does not launch twice; changed payload rejects;
- input wait and durable reply correlation survive a lost live connection.
- legitimate nested-child replies reach live waiters and survive restart with
  same-ID retry; forged immutable metadata, missing/wrong command or thread,
  invalid reply ID/hop, cross-owner/device senders, raw synced queue bypass and
  changed child ownership before dispatch reject without reaching a harness.

Then build a dedicated test binary remotely (`cargo build -p comet --bin comet`),
without replacing any installed binary/service. Linux UI build prerequisites are
listed in `.github/workflows/release.yml`: pkg-config, cmake, X11/XCB, Wayland,
xkbcommon, fontconfig/freetype, ALSA and Vulkan development packages. macOS needs
Xcode command-line tools. A real OMP/native smoke additionally requires an
already authorized Crew local-controller engine/agent route, supported installed
OMP and the exact model, with provider interactive approvals honored. Do not copy
credentials or create auth services to make the smoke pass.

Exercise the new CLI against that dedicated engine: ensure a worker in an
explicit disposable Git project, disconnect/retry ensure, inspect the actual
`WatchChats` projection (and native UI where available), send one harmless
instruction with stable ID, observe live/input/terminal states, reconnect and
read durable reply/Done, restart only that disposable engine and re-read
config/ownership, preserve a dirty file through interrupt/close/recover, reject a
foreign owner and a changed model/checkout, then verify ordinary handoff remains
available with the upstream implementation. Remote execution and real inference
need explicit authorization; none is claimed by this document.
