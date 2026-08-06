# Ashler Comet local and Scaffold end state

Status: acceptance contract

This document defines the required product boundary for Ashler Comet. A partial
"Comet binary installed in Scaffold" integration does not satisfy it.

## Product requirements

### Local Comet

1. The headed native Comet app and its local engine discover existing sessions
   owned by the current machine for:
   - Codex;
   - Claude Code;
   - OMP.
2. Comet can create new sessions for all three harnesses.
3. Imported or attached sessions retain their harness-native session id, cwd,
   provider/model metadata when available, and origin. Attaching is idempotent;
   Comet must not replay an existing turn or duplicate a transcript.
4. The machine running the harness remains the authoritative executor. Comet
   mirrors transcript, lifecycle, commands, approvals, tool results, diffs, and
   continuation state through its existing Cloudflare SessionRoom and DeviceRoom
   Durable Objects.
5. Any authorized Comet client can reconnect to that mirrored session and
   continue it while the owning machine is online. An offline owner remains
   readable from durable state but cannot claim that commands executed.
6. Local Comet exposes Scaffold environments through its native environment
   surface. It can create/inspect, pause, resume, stop, and attach to a running
   Scaffold OMP session without creating a parallel session backend.

### Future web and mobile viewports

Web and mobile clients are explicitly out of scope for the initial Comet/OMP
Scaffold rollout. The initial implementation must remain compatible with adding
them later as authorized Comet clients: keep existing Durable Objects and typed
commands as the shared data/control plane, keep execution on the owning local or
Scaffold host, preserve exact deployment/session/device/epoch authority, and do
not expose host, bootstrap, provider, or OMP credentials to observer clients.

The saved future architecture, mobile alternative, delivery slices, acceptance
matrix, and open questions live in
`internal/scaffold/docs/comet-web-viewport.md` in the platform repository.
They do not gate Comet 0.1.23 or the initial no-webview staging acceptance.

### Scaffold Comet

1. A Scaffold agent sandbox runs a headless Comet engine as its collaboration
   and session UI layer. The browser OpenCode view is not the authoritative UI.
2. OMP over ACP is the default and only user-selectable harness in the Scaffold
   Comet runtime profile.
3. The remote profile disables incompatible local-controller features:
   - creating or attaching to nested Scaffold environments;
   - selecting or starting Codex directly;
   - selecting or starting Claude Code directly;
   - importing host-machine sessions or credentials;
   - changing the deployment/project/device bootstrap identity.
4. The sandbox receives a short-lived, exact-scope bootstrap through a mode-0600
   single-use file. It never receives the local user's long-lived Scaffold OAuth
   bearer or Comet session credential.
5. Scaffold owns sandbox lifecycle. The Comet host owns OMP session lifecycle
   inside that sandbox. Pause/stop/wake/resume reconcile both layers without
   inventing completion or spawning duplicate OMP processes.
6. The remote Comet host publishes into the same SessionRoom/DeviceRoom contracts
   as local hosts. There is no T3, Session Fabric, or OpenCode transcript path.

## Runtime profiles

Comet must make capabilities server-bound rather than hiding unsupported buttons
only in the UI.

- `local-controller`: headed or headless local install; local Codex, Claude Code,
  and OMP; Scaffold environment control and attach are allowed.
- `scaffold-host`: deployment-bound headless engine; OMP only; recursive Scaffold,
  Codex, Claude Code, local session import, account switching, and credential
  management are rejected by the engine/RPC layer.
- `mock`: deterministic offline tests only.

A persisted chat records its execution host identity and harness. Resuming on a
host with the wrong runtime profile, cwd, deployment, sandbox, or lifecycle epoch
fails closed.

## Durable mirroring invariants

1. SessionRoom is the durable, multi-client session projection and semantic
   command inbox.
2. DeviceRoom is the exclusive live host and scoped RPC relay.
3. Only the authenticated host publishes harness events, receipts, lifecycle,
   and continuation ids. Clients submit role-authorized semantic commands.
4. Every command has actor, idempotency key, expiry, configuration revision, and
   run epoch. Reconnect and replay do not duplicate a turn.
5. Two clients observing one local or Scaffold host see one ordered transcript.
6. A client disconnect never stops the harness. A host disconnect makes the
   session offline/recoverable until the same host identity reconnects.
7. Durable history remains readable after a Scaffold sandbox is stopped or
   destroyed; further execution is rejected when its host identity cannot
   return.
8. A deployment-scoped session room key is exactly
   `s4/{projectId}/{deploymentId}/{sessionId}`. Two deployments using the same
   session id must never share transcript, tail, diff, grant, or attachment
   state.
9. A `scaffold-host` does not expose generic `ListHarnesses` or `ListModels`
   RPCs. The trusted bootstrap fixes OMP and its model gateway; discovery is a
   local-controller capability, not a remotely selectable sandbox surface.

## Implementation slices

### A. Runtime capability profiles

Add a typed runtime profile/capability set to the Comet engine, RPC, and UI boot
configuration. Filter UI catalogs for usability, but also reject prohibited RPC
and harness operations in the engine. Cover profile serialization and negative
RPC tests first.

### B. OMP ACP harness

Add `HarnessId::Omp` and an adapter that spawns `omp acp`, implementing ACP
initialize, session/new, session/load or resume, prompt, streaming updates,
approvals/input, cancellation, model/config discovery, and deterministic terminal
status. Store OMP's native session id and cwd for continuation. Package a pinned
OMP runtime in the Scaffold session image and make it the `scaffold-host` default.

### C. Existing-session discovery and import

Add provider-specific discovery adapters:

- OMP: native session index and ACP load/resume;
- Codex: local Codex thread/session store and app-server resume;
- Claude Code: local project/session store and `--resume` continuation.

Normalize discoveries into read-only candidates before explicit attach. Import
history with stable source ids and a per-source cursor; never infer completed
commands from untrusted local files. Watch for new events only through the owned
harness process or its supported protocol.

### D. Native Scaffold environment UX

Connect the existing `ScaffoldRuntime` RPC methods to the GPUI application:
environment watch/refresh; create/inspect; pause/resume/stop; attach. Selecting
attach mints the exact device grant, starts/reconciles the sandbox Comet host,
and opens the same mirrored session in the native UI.

### E. Scaffold host lifecycle and OpenCode removal

Add an explicit Scaffold/Comet agent runtime profile in the platform. The
provider/supervisor owns one headless Comet process, health/sync state, restart,
and OMP child lifecycle. Remove OpenCode process, port 4096 attach rewrites,
OpenCode completion/activity/handoff state, and browser Agent view from this
profile. Retain the old profile only as a time-bounded rollback until acceptance
passes, then delete it.

### F. Staged cutover

Deploy in dependency order: Comet edge staging; pinned Comet and OMP artifacts;
Scaffold image/template; provider/Worker; staging default profile; live
multi-client proof. Production remains gated on the full acceptance matrix.

## Acceptance tests

### Local harnesses

For OMP, Codex, and Claude Code independently:

1. create a native session and complete a unique first turn;
2. discover it from a fresh Comet client/data process;
3. attach without duplicating history;
4. continue with a unique second turn;
5. observe both turns from a second authorized Comet client;
6. disconnect/reconnect the observing client during streaming;
7. restart Comet and resume by the same native session id and cwd;
8. reject a mismatched cwd or execution host.

### Local controller to Scaffold

1. list and create a Scaffold environment from native Comet;
2. observe preparing to ready from events, not polling in product code;
3. attach to its already-running OMP session;
4. pause, resume, and stop from the local native client;
5. prove idempotent commands and one OMP process;
6. prove an unauthorized actor and wrong project/deployment/device fail closed.

### Scaffold host restrictions

1. runtime reports `scaffold-host` and OMP as the only available harness;
2. attempts to start Codex or Claude Code fail at the engine boundary;
3. attempts to list/create nested Scaffold environments fail at the engine
   boundary;
4. no OpenCode process or port 4096 listener exists;
5. no local account/session-import surface is available;
6. bootstrap is mode 0600, absent from argv/logs, consumed before exchange, and
   deleted on success and every failure.

### Multi-client mirroring

Run the same scenario once with a local host and once with a Scaffold host:

1. two clients open the same chat;
2. client A submits a turn and both receive ordered streamed deltas;
3. client B steers or answers an approval and both receive the authoritative
   receipt;
4. disconnect A, continue from B, reconnect A, and converge without duplicates;
5. disconnect the host, retain readable durable history, reject execution while
   offline, reconnect the same host, and drain exactly one queued command;
6. revoke a client or host grant and prove the open socket closes and reconnect
   fails;
7. create two deployments with the same session id and prove their Durable
   Object state is isolated.

### Scaffold release gate

A staging candidate passes only when a fresh sandbox proves: pinned Comet and OMP
versions; no unresolved shared libraries; OpenCode absent; OMP completes and
resumes; native local Comet attaches and controls lifecycle; two clients mirror
through staging Durable Objects; primary Scaffold state remains unchanged.
