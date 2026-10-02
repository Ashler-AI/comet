/**
 * SessionRoom — one Durable Object per doc room, speaking loro-protocol over
 * hibernatable WebSockets (design §2, §3.1). Chat and workspace documents use
 * deterministic room names derived from the verified Scaffold scope; the DO binds
 * the scope on first use and rejects cross-scope access.
 *
 * Persistence model:
 * - `updates` — accepted incoming deltas, written transactionally before ACK.
 *   storage.sync() is the durable boundary; publisher and edge may both restart.
 * - `snapshot` blob — the doc's current snapshot. Two-level compaction:
 *   LOG FOLD (whenever the update log passes COMPACT_LOG_BYTES): re-export a
 *   full snapshot and clear the log — loses nothing. HISTORY TRIM applies only
 *   to session transcripts: it retains current state and a shallow causal
 *   boundary; older peers take the stale-peer full-resync path. Workspace and
 *   unknown legacy rooms retain every causal operation for offline writers.
 * - `tail` blob — materialized last-N-messages JSON, recomputed lazily on
 *   GET /tail when dirty (§5 L2).
 * - `diff` blob — latest-only working-tree diff sidecar, overwritten on each
 *   host publish (§6.1).
 * - Ephemeral presence (%EPH room) is memory-only by construction.
 *
 * Hibernation discipline: timers only release idle caches and fragment batches;
 * scheduled work
 * (checkpoints, history trim, R2 backup §3.3) rides the durable alarm.
 */
import { LoroDoc, EphemeralStore, VersionVector, decodeImportBlobMeta } from "loro-crdt";
import type { PeerID } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MAX_MESSAGE_SIZE,
  MessageType,
  UpdateStatusCode,
  bytesToHex,
  decode,
  encode,
  type DocUpdate,
  type DocUpdateFragmentHeader,
  type JoinRequest,
  type ProtocolMessage
} from "loro-protocol";
import {
  COMPACT_LOG_BYTES,
  COMPACT_LOG_ROWS,
  RETAIN_DAYS,
  materializeTail
} from "./session-doc";
import { CHUNK_BYTES, createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import {
  AUTH_GRANT_HEADER,
  AUTH_USER_HEADER,
  GRANT_EVENT_HEADER,
  ROOM_KIND_HEADER,
  SESSION_OWNER_AUTH_HEADER,
  NOTIFICATION_BEARER_HEADER,
  type Env
} from "./env";
import { parseTrustedDeviceGrant } from "./device-room";
import { WorkspaceNotifications } from "./notifications";

const AUTH_PROJECT_HEADER = "x-comet-auth-project";
const AUTH_CAPABILITIES_HEADER = "x-comet-auth-capabilities";
const SESSION_READ = "session.read";
const GRANT_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const PUBLISH_CAPABILITIES: Record<string, true> = {
  "session.chat": true,
  "session.control": true,
  "session.annotate": true,
  "session.files": true
};
const parseCapabilities = (request: Request): string[] =>
  (request.headers.get(AUTH_CAPABILITIES_HEADER) ?? "").split(/\s+/).filter(Boolean);
const canPublish = (capabilities: readonly string[]): boolean =>
  capabilities.some((capability) => PUBLISH_CAPABILITIES[capability] === true);

const SESSION_OWNER_AUTH_VALUE = "verify";

export const sessionOwnerForConnection = (
  currentOwnerUserId: string | undefined,
  userId: string,
  workspace: boolean,
  device: boolean
): string | undefined =>
  currentOwnerUserId ?? (!workspace && !device ? userId : undefined);

const SESSION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The sole name accepted for a globally shared session room. */
export const canonicalSessionId = (value: string | undefined): string | undefined => {
  if (!value || !SESSION_ID_RE.test(value)) return undefined;
  return value.toLowerCase();
};

const DAY_MS = 24 * 60 * 60 * 1000;
const RETAIN_MS = RETAIN_DAYS * DAY_MS;
/** Session-only replay reset policy. Workspace history is never reset or gated. */
const REPLAY_CRASH_LIMIT = 3;
const FRAGMENT_TTL_MS = 30_000;
/** Payload bytes per outbound fragment (leaves room for the envelope). */
const FRAGMENT_BYTES = 200_000;
/** Match the sync client's healthy-snapshot limits, shared across each
 * socket's in-flight batches so incomplete headers cannot accumulate forever. */
const MAX_REASSEMBLED_BYTES = 64 * 1024 * 1024;
const MAX_FRAGMENT_COUNT = 1024;
const MAX_PRESENCE_UPDATE_BYTES = 16 * 1024;
const WORKSPACE_PRESENCE_TTL_MS = 30_000;
const MAX_WORKSPACE_PRESENCE_PEERS = 128;
/** Atomic workspace recovery accepts one already-compacted canonical snapshot. */
const MAX_RESET_SEED_BYTES = 8 * 1024 * 1024;

const readBoundedBody = async (request: Request, maxBytes: number): Promise<Uint8Array | null> => {
  const advertised = Number(request.headers.get("content-length"));
  if (Number.isFinite(advertised) && advertised > maxBytes) return null;
  const reader = request.body?.getReader();
  if (!reader) return new Uint8Array();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (size + value.byteLength > maxBytes) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
      size += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }
  if (chunks.length === 1) return chunks[0]!;
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
};

/** Validates the typed JSON carried inside a Loro ephemeral participant value. */
export const isValidParticipantCursor = (cursor: unknown, text?: string): boolean => {
  if (!cursor || typeof cursor !== "object") return false;
  const value = cursor as Record<string, unknown>;
  if (
    typeof value.targetId !== "string" ||
    value.targetId.length === 0 ||
    new TextEncoder().encode(value.targetId).length > 256 ||
    !Number.isSafeInteger(value.caret) ||
    (value.caret as number) < 0 ||
    (value.caret as number) > 16 * 1024 * 1024
  ) {
    return false;
  }
  let start = value.caret as number;
  let end = start;
  if (value.selection !== undefined) {
    if (!value.selection || typeof value.selection !== "object") return false;
    const selection = value.selection as Record<string, unknown>;
    if (
      !Number.isSafeInteger(selection.start) ||
      !Number.isSafeInteger(selection.end) ||
      (selection.start as number) < 0 ||
      (selection.start as number) > (selection.end as number) ||
      (selection.end as number) > 16 * 1024 * 1024 ||
      (value.caret as number) < (selection.start as number) ||
      (value.caret as number) > (selection.end as number)
    ) {
      return false;
    }
    start = selection.start as number;
    end = selection.end as number;
  }
  if (text === undefined) return true;
  const bytes = new TextEncoder().encode(text);
  if (end > bytes.length) return false;
  try {
    const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });
    decoder.decode(bytes.slice(0, start));
    decoder.decode(bytes.slice(0, value.caret as number));
    decoder.decode(bytes.slice(0, end));
  } catch {
    return false;
  }
  return true;
};
/** Keep a rolling ~5 weeks of daily frontier checkpoints. */
const MAX_CHECKPOINTS = 36;
/** Isolate-wide poisoned-wasm strike counter — MODULE state on purpose: every
 * SessionRoom co-located in this isolate shares ONE loro-wasm linear memory,
 * so heap exhaustion poisons them all at once (2026-08-04: every
 * byte-exporting wasm call threw RangeError("Invalid array buffer length")
 * while imports/relays kept working, silently wedging all joins fleet-wide). */
let wasmPoisonStrikes = 0;
/** Wasm-boundary failures before `ctx.abort()` recycles the isolate. Wasm
 * memory only ever grows, so the poisoned state is permanent until the isolate
 * dies — and Cloudflare's own memory-limit reset arrives only after minutes of
 * thrash. Aborting early turns a silent hours-long wedge into a seconds-long
 * blip (sockets close, clients redial into a fresh isolate). */
const WASM_POISON_ABORT_AFTER = 3;
/** Free a quiet room's materialized doc after this long. Wasm linear memory
 * NEVER shrinks and outlives DO instances (one wasm module per isolate), so
 * docs held resident until instance eviction leak permanently — every
 * reconnect herd rematerialized every co-located room and the heap climbed
 * monotonically to the isolate limit in minutes (2026-08-04 thrash loop).
 * Freeing on idle returns blocks to the wasm allocator for reuse;
 * rematerialization is a 10-50ms cold replay. */
const DOC_IDLE_RELEASE_MS = 60_000;
/** Session-only force-trim threshold: a room with NO eligible checkpoint but a
 * full-history snapshot this large trims at its CURRENT frontier instead of
 * waiting days to age into RETAIN_DAYS eligibility. Behind/concurrent peers
 * take the §3.1 stale-peer full resync (designed-for). Without this, the
 * 2026-08-04 whale rooms (954KB / 1.8MB import chats, checkpoints first
 * recorded today) would have kept re-materializing their full history into
 * the pressed wasm heap for three more days of thrash. */
const TRIM_FORCE_BYTES = 512 * 1024;

export interface SocketGrantState {
  grantId?: string;
  grantExpiresAt?: number;
}

export const enforceDeviceGrantAuthority = async (
  ws: Pick<WebSocket, "close">,
  state: SocketGrantState | null,
  now: number,
  validate: (grantId: string) => Promise<boolean>
): Promise<boolean> => {
  const grantId = state?.grantId;
  const grantExpiresAt = state?.grantExpiresAt;
  const hasGrantState = grantId !== undefined || grantExpiresAt !== undefined;
  let valid = state !== null;
  if (valid && hasGrantState) {
    if (
      typeof grantId !== "string" ||
      !GRANT_ID_RE.test(grantId) ||
      typeof grantExpiresAt !== "number" ||
      !Number.isSafeInteger(grantExpiresAt) ||
      grantExpiresAt <= now
    ) {
      valid = false;
    } else {
      try {
        valid = (await validate(grantId)) === true;
      } catch {
        valid = false;
      }
    }
  }
  if (valid) return true;
  try {
    ws.close(4403, "device grant invalid");
  } catch {
    /* already gone */
  }
  return false;
};

interface SocketState extends SocketGrantState {
  userId: string;
  projectScope: string;
  capabilities: string[];
  /** Joined sub-rooms by crdt magic ("%LOR", "%EPH"). */
  rooms: string[];
  /** True for sockets on a project-workspace document. */
  workspace?: boolean;
  /** Authorized recovery upload, not membership or a converged join. */
  loroRecoveryRoomId?: string;
  /** Dialing engine's device id (from `&device=`, Worker-validated) — pure
   * log attribution; never used for authz. */
  deviceId?: string;
}

interface FragmentBatch {
  parts: Array<Uint8Array | undefined>;
  received: number;
  receivedBytes: number;
  totalSize: number;
  header: DocUpdateFragmentHeader;
  expiresAt: number;
}
interface WorkspacePresenceEntry {
  expiresAt: number;
  updates: Uint8Array[];
}

interface FrontierCheckpoint {
  at: number;
  frontiers: { peer: string; counter: number }[];
}

export class SessionRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  private notifications: WorkspaceNotifications | undefined;
  /** Lazily materialized doc — the log is authoritative; this is a cache. */
  private doc: LoroDoc | undefined;
  private docLoad: Promise<LoroDoc> | undefined;
  /** Legacy accepted deltas may still lack dependencies after cold replay. */
  private docPendingEnds: Map<PeerID, number> | undefined;
  private eph: EphemeralStore | undefined;
  /** Bounded byte snapshots for workspace presence. Avoids another Loro WASM
   * allocation while letting fresh joiners observe peers before the next
   * 15-second heartbeat. Lost on hibernation by design. */
  private readonly workspacePresence = new Map<string, WorkspacePresenceEntry>();
  /** In-memory fragment reassembly. Lost on hibernation → the sender gets a
   * FragmentTimeout ack for the unknown batch and resends — self-healing. */
  private readonly fragments = new Map<WebSocket, Map<string, FragmentBatch>>();
  private fragmentTimer: number | undefined;
  /** Revocations delivered while this instance is live close the TOCTOU gap
   * between an authority response and a handler's final mutation/send. */
  private readonly revokedGrants = new Set<string>();
  /** Idle-doc release bookkeeping (see DOC_IDLE_RELEASE_MS / touchDoc). */
  private docIdleTimer: ReturnType<typeof setTimeout> | undefined;
  private lastDocUse = 0;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    // Protocol-designed hibernation keepalive: ping → pong without waking us.
    // NOTE (2026-07-30 incident): precisely BECAUSE the runtime answers these
    // itself, a pong is NOT evidence this DO can still run — a wedged room
    // kept auto-ponging for hours while never processing a join. Clients judge
    // room liveness from protocol frames plus a join-response deadline
    // (crates/sync/src/room.rs), never from these pongs. Do not "upgrade" this
    // to an app-level handler: waking on every ping would abolish hibernation.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/grant-revoked" && request.method === "POST") {
      if (request.headers.get(GRANT_EVENT_HEADER) !== "revoke") {
        return new Response("forbidden", { status: 403 });
      }
      let body: unknown;
      try {
        body = await request.json();
      } catch {
        return new Response("invalid request", { status: 400 });
      }
      if (
        !body ||
        typeof body !== "object" ||
        !("grantId" in body) ||
        typeof body.grantId !== "string" ||
        !GRANT_ID_RE.test(body.grantId)
      ) {
        return new Response("invalid request", { status: 400 });
      }
      await this.revokeGrant(body.grantId);
      return new Response(null, { status: 204 });
    }
    if (url.pathname === "/authorize-owner") {
      if (request.headers.get(SESSION_OWNER_AUTH_HEADER) !== SESSION_OWNER_AUTH_VALUE) {
        return new Response("forbidden", { status: 403 });
      }
      if (request.method !== "GET") {
        return new Response("method not allowed", { status: 405 });
      }
      const candidateUserId = request.headers.get(AUTH_USER_HEADER);
      const candidateProjectScope = request.headers.get(AUTH_PROJECT_HEADER);
      if (!candidateUserId || !candidateProjectScope) {
        return new Response("unauthenticated", { status: 401 });
      }
      const ownsSession =
        this.getMeta("projectScope") === candidateProjectScope &&
        this.getMeta("ownerUserId") === candidateUserId;
      if (!ownsSession) return json({ ownsSession: false });
      let deviceId = this.getMeta("hostDeviceId");
      if (!deviceId) {
        const liveDevices = new Set(
          this.ctx.getWebSockets()
            .map((socket) => (socket.deserializeAttachment() as SocketState | null)?.deviceId)
            .filter((value): value is string => typeof value === "string" && GRANT_ID_RE.test(value))
        );
        if (liveDevices.size === 1) {
          deviceId = liveDevices.values().next().value;
          if (deviceId) this.setMeta("hostDeviceId", deviceId);
        }
      }
      return json({ ownsSession: true, ...(deviceId ? { deviceId } : {}) });
    }
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const projectScope = request.headers.get(AUTH_PROJECT_HEADER);
    const capabilities = parseCapabilities(request);
    if (!projectScope || !capabilities.includes(SESSION_READ)) {
      return new Response("forbidden", { status: 403 });
    }
    const boundScope = this.getMeta("projectScope");
    if (!boundScope) this.setMeta("projectScope", projectScope);
    else if (boundScope !== projectScope) return new Response("forbidden", { status: 403 });
    // Workspace routing was scope-checked by the Worker; this DO independently
    // binds the same verified project scope above.
    const workspace = request.headers.get(ROOM_KIND_HEADER) === "workspace";
    if (workspace || !this.getMeta("roomKind")) this.setMeta("roomKind", workspace ? "workspace" : "session");
    if (url.pathname === "/notifications/device") {
      if (!workspace || request.headers.has(AUTH_GRANT_HEADER)) return json({ error: "forbidden" }, 403);
      const bearer = request.headers.get(NOTIFICATION_BEARER_HEADER);
      if (!bearer) return json({ error: "forbidden" }, 403);
      return this.workspaceNotifications().register(request, userId, bearer);
    }

    if (url.pathname === "/ws") {
      const chatId = url.searchParams.get("chatId") ?? "";
      const encodedGrant = request.headers.get(AUTH_GRANT_HEADER);
      const grant =
        encodedGrant === null
          ? undefined
          : parseTrustedDeviceGrant(encodedGrant, userId, projectScope, Date.now());
      if (encodedGrant !== null && (!grant || (workspace
        ? chatId !== `ws4/${projectScope}`
        : grant.scope.sessionId !== chatId))) {
        return new Response("forbidden", { status: 403 });
      }
      if (grant && !workspace) {
        const boundDeploymentId = this.getMeta("deploymentId");
        const boundSessionId = this.getMeta("scopedSessionId");
        if (
          (boundDeploymentId && boundDeploymentId !== grant.scope.deploymentId) ||
          (boundSessionId && boundSessionId !== grant.scope.sessionId)
        ) {
          return new Response("forbidden", { status: 403 });
        }
        if (!boundDeploymentId) this.setMeta("deploymentId", grant.scope.deploymentId);
        if (!boundSessionId) this.setMeta("scopedSessionId", grant.scope.sessionId);
      }
      const currentOwnerUserId = this.getMeta("ownerUserId");
      const ownerUserId = sessionOwnerForConnection(
        currentOwnerUserId,
        userId,
        workspace,
        grant !== undefined
      );
      if (!currentOwnerUserId && ownerUserId) this.setMeta("ownerUserId", ownerUserId);
      if (chatId && !this.getMeta("chatId")) this.setMeta("chatId", chatId);
      const deviceId = url.searchParams.get("device") ?? undefined;
      if (!workspace && ownerUserId && deviceId && GRANT_ID_RE.test(deviceId) && !this.getMeta("hostDeviceId")) {
        this.setMeta("hostDeviceId", deviceId);
      }
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = {
        userId,
        projectScope,
        capabilities,
        rooms: [],
        ...(workspace ? { workspace } : {}),
        ...(deviceId ? { deviceId } : {}),
        ...(grant
          ? { grantId: grant.grantId, grantExpiresAt: grant.expiresAt }
          : {})
      };
      pair[1].serializeAttachment(state);
      await this.scheduleGrantExpiryAlarm();
      console.log(
        "socket accepted",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${deviceId ?? "unattributed"}`,
        `sockets=${this.ctx.getWebSockets().length}`
      );
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    const owner = this.getMeta("owner");

    if (url.pathname === "/stats" && request.method === "GET") {
      // Observability: what this room holds and who's on it. Owner-gated like
      // every other read (org-membership-gated for workspace rooms).
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      await this.flush();
      const updateRows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]
        ?.n as number;
      return json({
        chatId: this.getMeta("chatId") ?? null,
        connectedSockets: this.ctx.getWebSockets().length,
        updateRows,
        updateLogBytes: Number(this.getMeta("updateBytes") ?? "0"),
        snapshotBytes: this.blobs.byteLength("snapshot") ?? 0,
        // Cold-start cost of the LAST materialization — the wedge-risk gauge
        // (2026-07-30: this creeping toward the CPU limit was invisible).
        lastReplayMs: Number(this.getMeta("lastReplayMs") ?? "0"),
        lastReplayRows: Number(this.getMeta("lastReplayRows") ?? "0"),
        lastReplayBatches: Number(this.getMeta("lastReplayBatches") ?? "0"),
        lastColdMs: Number(this.getMeta("lastColdMs") ?? "0"),
        // True between a wedge-break log drop and the first re-uploaded state
        // (the nightly backup is paused in that window).
        postReset: this.getMeta("postReset") === "1",
        tailCached: this.getMeta("tailDirty") !== "1" && this.blobs.byteLength("tail") !== undefined,
        diffPublished: this.blobs.byteLength("diff") !== undefined,
        checkpoints: (JSON.parse(this.getMeta("checkpoints") ?? "[]") as unknown[]).length,
        lastTrimAt: this.getMeta("lastTrimAt") ?? null,
        backupDirty: this.getMeta("backupDirty") === "1",
        // Non-zero while a cold replay is in flight or has been dying — the
        // wedge signature ensureDoc's automated reset watches for.
        replayAttempts: Number(this.getMeta("replayAttempts") ?? "0")
      });
    }
    if (url.pathname === "/tail" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      return json(await this.currentTail());
    }
    if (url.pathname === "/diff" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      const diff = getJsonBlob<unknown>(this.blobs, "diff");
      return diff === undefined ? json({ error: "not_found" }, 404) : json(diff);
    }
    if (url.pathname === "/diff" && request.method === "POST") {
      if (!capabilities.includes("session.files")) {
        return json({ error: "forbidden" }, 403);
      }
      // The host may publish before any room join has claimed the doc.
      if (!workspace) {
        if (!owner) this.setMeta("owner", projectScope);
        else if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      putJsonBlob(this.blobs, "diff", await request.json());
      return json({ ok: true });
    }
    if (url.pathname === "/snapshot" && request.method === "GET") {
      // Repair/inspection read: the doc's full current snapshot bytes.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      await this.flush();
      const doc = await this.ensureDoc();
      this.assertLoroMaterialized(doc, this.docPendingEnds);
      const bytes = doc.export({ mode: "snapshot" });
      return new Response(bytes as unknown as BodyInit, {
        headers: { "content-type": "application/octet-stream" }
      });
    }
    if (url.pathname === "/append" && request.method === "POST") {
      if (!capabilities.includes("session.chat")) {
        return json({ error: "forbidden" }, 403);
      }
      // MERGE-safe repair write: import a Loro update (never replaces the
      // doc). Same durability bookkeeping as a WS DocUpdate.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      const body = new Uint8Array(await request.arrayBuffer());
      let doc = await this.ensureDoc();
      if (workspace) this.notifyWorkspace(doc, projectScope, true);
      try {
        doc = this.importLoroUpdates(doc, [body]);
      } catch (error) {
        this.escalateWasmPoisoning(error);
        return json({ error: "invalid_update" }, 400);
      }
      if (workspace) this.notifyWorkspace(doc, projectScope);
      await this.ctx.storage.sync();
      // Converge live peers: relay the update to connected %LOR sockets.
      const roomId = this.getMeta("chatId") ?? "";
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment() as SocketState | null;
        if (!state?.rooms.includes(CrdtType.Loro)) continue;
        if (!(await this.authorizeSocket(ws, state))) continue;
        this.sendUpdates(ws, CrdtType.Loro, roomId, [body]);
      }
      return json({ ok: true });
    }
    if (url.pathname === "/reset-log" && request.method === "POST") {
      if (!capabilities.includes("session.control")) {
        return json({ error: "forbidden" }, 403);
      }
      // WEDGE BREAK: replace the persisted workspace state without first
      // materializing the potentially oversized old document. An optional
      // bounded complete snapshot makes recovery deterministic when several
      // clients have incompatible shallow-history boundaries; an empty body
      // retains the legacy clear-and-reupload behavior.
      if (!workspace) {
        if (!owner) return json({ error: "not_found" }, 404);
        if (owner !== projectScope) return json({ error: "forbidden" }, 403);
      }
      const seed = await readBoundedBody(request, MAX_RESET_SEED_BYTES);
      if (seed === null) return json({ error: "too_large" }, 413);
      if (seed.byteLength > 0) {
        if (!workspace) return json({ error: "workspace_seed_required" }, 400);
        let validationStage = "decode";
        try {
          const metadata = decodeImportBlobMeta(seed, false);
          validationStage = "mode";
          try {
            if (metadata.mode !== "snapshot" && metadata.mode !== "shallow-snapshot" && metadata.mode !== "outdated-snapshot") {
              return json({ error: "complete_snapshot_required" }, 400);
            }
          } finally {
            validationStage = "free-start";
            metadata.partialStartVersionVector.free();
            validationStage = "free-end";
            metadata.partialEndVersionVector.free();
          }
        } catch (error) {
          this.escalateWasmPoisoning(error);
          if (this.env.ENVIRONMENT === "staging") {
            const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", seed as Uint8Array<ArrayBuffer>));
            return json({
              error: "invalid_snapshot", validationStage, seedBytes: seed.byteLength,
              sha256: Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(""),
              reason: (error instanceof Error ? error.stack ?? `${error.name}: ${error.message}` : String(error)).slice(0, 512)
            }, 400);
          }
          return json({ error: "invalid_snapshot" }, 400);
        }
      }
      const before = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
        | number
        | undefined;
      this.ctx.storage.transactionSync(() => {
        this.dropLog();
        if (seed.byteLength > 0) {
          this.blobs.put("snapshot", seed);
          this.setMeta("postReset", "0");
          this.setMeta("tailDirty", "1");
          this.setMeta("backupDirty", "1");
        }
        this.setMeta("replayAttempts", "0");
      });
      this.doc?.free(); // release the wasm memory, don't wait on GC finalizers
      this.doc = undefined; // force materialization from the replacement state
      this.closeSocketsForRoomReset();
      return json({ ok: true, clearedUpdateRows: before ?? 0, seedBytes: seed.byteLength });
    }
    return new Response("not found", { status: 404 });
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async revokeGrant(grantId: string): Promise<void> {
    this.revokedGrants.add(grantId);
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment() as SocketState | null;
      if (state?.grantId !== grantId) continue;
      try {
        ws.close(4403, "device grant revoked");
      } catch {
        /* already gone */
      }
    }
  }

  private async authorizeSocket(
    ws: WebSocket,
    state: SocketState | null
  ): Promise<boolean> {
    return enforceDeviceGrantAuthority(ws, state, Date.now(), async (grantId) => {
      if (this.revokedGrants.has(grantId)) return false;
      const stub = this.env.AUTH_GRANTS.get(
        this.env.AUTH_GRANTS.idFromName(grantId)
      );
      const response = await stub.fetch(
        new Request(`https://grant.internal/status?grantId=${encodeURIComponent(grantId)}`, {
          headers: { [GRANT_EVENT_HEADER]: "status" }
        })
      );
      return response.ok && !this.revokedGrants.has(grantId);
    });
  }

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    const attached = ws.deserializeAttachment() as SocketState | null;
    if (!(await this.authorizeSocket(ws, attached))) return;
    if (typeof message === "string") return; // ping/pong handled by auto-response
    const state = attached as SocketState;
    let decoded: ProtocolMessage;
    try {
      decoded = decode(new Uint8Array(message));
    } catch {
      ws.close(1002, "Protocol error");
      return;
    }
    try {
      switch (decoded.type) {
        case MessageType.JoinRequest:
          await this.handleJoin(ws, state, decoded);
          break;
        case MessageType.DocUpdate:
          await this.handleDocUpdate(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragmentHeader:
          this.handleFragmentHeader(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragment:
          await this.handleFragment(ws, state, decoded);
          break;
        case MessageType.Leave:
          state.rooms = state.rooms.filter((r) => r !== decoded.crdt);
          ws.serializeAttachment(state);
          break;
        case MessageType.Ack:
        case MessageType.RoomError:
          break;
        default:
          ws.close(1002, "Unsupported message");
      }
    } catch (e) {
      // A handler that dies pre-answer used to fail in SILENCE: the client
      // waits out its 15s join deadline, redials, and dies the same way —
      // the 2026-08-04 fleet-wide join wedge. Log attributed, answer an
      // outstanding join so clients fail fast and VISIBLY (JoinError →
      // long-backoff rejoin instead of a hot 15s dial loop), then escalate
      // suspected wasm-heap poisoning to an isolate recycle.
      console.error(
        "ws message handler failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state?.deviceId ?? "unattributed"}`,
        `type=${decoded.type}`,
        String(e)
      );
      if (decoded.type === MessageType.JoinRequest) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: decoded.crdt,
          roomId: decoded.roomId,
          code: JoinErrorCode.AppError,
          message: "internal error"
        });
      }
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    this.logSocketEnd(ws, "closed");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      // Flush can fold the log (a wasm snapshot export); an uncaught throw
      // here is invisible in a close handler. Same discipline as above.
      console.error("flush on socket close failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.logSocketEnd(ws, "errored");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket error failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }

  /** RangeError("Invalid array buffer length") / wasm RuntimeError are the
   * signature of an exhausted loro-wasm heap (see `wasmPoisonStrikes`).
   * Strike out and `ctx.abort()` so clients redial into a fresh isolate
   * within seconds instead of hot-looping against a deaf room until
   * Cloudflare's memory-limit reset finally fires. */
  private escalateWasmPoisoning(e: unknown): void {
    if (this.env.ENVIRONMENT === "staging") {
      console.error("Loro operation failed", `room=${this.getMeta("chatId") ?? "?"}`,
        (e instanceof Error ? e.stack ?? `${e.name}: ${e.message}` : String(e)).slice(0, 512));
    }
    if (!(e instanceof RangeError || e instanceof WebAssembly.RuntimeError)) return;
    wasmPoisonStrikes++;
    if (wasmPoisonStrikes < WASM_POISON_ABORT_AFTER) return;
    console.error(`wasm heap poisoned (${wasmPoisonStrikes} strikes); aborting isolate for a fresh heap`);
    // Best effort: if abort recycles only the DO instance (not the whole
    // isolate), at least this room's doc goes back to the wasm allocator.
    try {
      this.doc?.free();
    } catch {
      /* already poisoned beyond freeing */
    }
    this.doc = undefined;
    this.ctx.abort("loro-wasm heap exhausted; recycling isolate");
  }

  private logSocketEnd(ws: WebSocket, how: string): void {
    const state = ws.deserializeAttachment() as SocketState | null;
    console.log(
      `socket ${how}`,
      `room=${this.getMeta("chatId") ?? "?"}`,
      `device=${state?.deviceId ?? "unattributed"}`,
      `sockets=${Math.max(0, this.ctx.getWebSockets().length - 1)}`
    );
  }

  private async handleJoin(ws: WebSocket, state: SocketState, message: JoinRequest): Promise<void> {
    // The room is bound to verified control-plane scope, not one user's identity.
    const owner = this.getMeta("owner");
    if (!owner) this.setMeta("owner", state.projectScope);
    else if (owner !== state.projectScope) {
      this.send(ws, {
        type: MessageType.JoinError,
        crdt: message.crdt,
        roomId: message.roomId,
        code: JoinErrorCode.AuthFailed,
        message: "scope does not own this room"
      });
      return;
    }
    if (!this.getMeta("chatId") && message.roomId) this.setMeta("chatId", message.roomId);
    if (state.workspace || !this.getMeta("roomKind")) this.setMeta("roomKind", state.workspace ? "workspace" : "session");

    if (message.crdt === CrdtType.Loro) {
      await this.ensureDoc();
      if (!(await this.authorizeSocket(ws, state))) return;
      if (ws.readyState !== WebSocket.OPEN) return;
      // Trimming frees the old doc while authority lookup yields; idle release
      // may remove it entirely. Do not retain the materialization result.
      const doc = this.doc;
      if (!doc) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AppError,
          message: "document released; rejoin"
        });
        return;
      }
      try {
        this.assertLoroMaterialized(doc, this.docPendingEnds);
      } catch {
        state.rooms = state.rooms.filter((room) => room !== CrdtType.Loro);
        state.loroRecoveryRoomId = canPublish(state.capabilities) ? message.roomId : undefined;
        ws.serializeAttachment(state);
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AppError,
          message: "incomplete_history"
        });
        return;
      }
      state.loroRecoveryRoomId = undefined;
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      // Wasm-bindgen objects (VersionVector here and below) free their wasm
      // memory only via GC finalizers — and V8 has no reason to collect when
      // the pressure is in WASM linear memory, not the JS heap. Under join
      // storms these leaked per-answer until the isolate hit its memory
      // limit (2026-08-04 exhaustion). Free explicitly.
      const vv = doc.oplogVersion();
      try {
        this.send(ws, {
          type: MessageType.JoinResponseOk,
          crdt: message.crdt,
          roomId: message.roomId,
          permission: canPublish(state.capabilities) ? "write" : "read",
          version: vv.encode()
        });
      } finally {
        vv.free();
      }
      let backfill: Uint8Array | undefined;
      if (message.version.length > 0) {
        let from: VersionVector | undefined;
        let retainedSince: VersionVector | undefined;
        try {
          from = VersionVector.decode(message.version);
          retainedSince = doc.shallowSinceVV();
          const coverage = from.compare(retainedSince);
          // Exporting from before retained history can succeed with missing
          // dependencies. Only a client covering the retained-history boundary
          // can use deltas; older or concurrent clients need the full state.
          if (from.length() > 0 && coverage !== undefined && coverage >= 0) {
            backfill = doc.export({ mode: "update", from });
          }
        } catch {
          // Unknown/garbled client version needs the persisted full baseline.
        } finally {
          from?.free();
          retainedSince?.free();
        }
      }
      if (backfill) {
        if (backfill.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      } else {
        // No await: snapshot, SQL rows, and buffered writes describe exactly
        // the advertised version. Send the lazy baseline FIRST, avoiding a
        // full-history WASM export merely to bootstrap a fresh mobile reader.
        const baseline = this.blobs.get("snapshot");
        if (baseline?.length && !this.sendUpdates(ws, message.crdt, message.roomId, [baseline])) return;
        for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
          if (!this.sendUpdates(ws, message.crdt, message.roomId, [new Uint8Array(row.bytes as ArrayBuffer)])) return;
        }
      }
      // The full join answer completed without a WASM failure.
      // Without this reset, occasional transient RangeErrors accumulated
      // over an isolate's lifetime and the tripwire aborted HEALTHY
      // isolates, each abort causing a reconnect herd that produced more
      // transient errors (observed 2026-08-04: ~1 abort/min with all rooms
      // already trimmed small). Poisoning is CONSECUTIVE failures.
      wasmPoisonStrikes = 0;
      return;
    }

    if (message.crdt === CrdtType.LoroEphemeralStore) {
      // Workspace presence uses bounded encoded snapshots instead of another
      // Loro WASM store beside the large workspace document. Each device
      // republishes every 15 seconds; entries expire at the client's 30-second
      // ephemeral timeout.
      const eph = state.workspace ? undefined : this.ensureEph();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: new Uint8Array()
      });
      if (state.workspace) {
        const cached = this.workspacePresenceSnapshot(state.deviceId, Date.now());
        if (cached.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, cached);
      } else if (eph) {
        const all = eph.encodeAll();
        if (all.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [all]);
      }
      return;
    }

    this.send(ws, {
      type: MessageType.JoinError,
      crdt: message.crdt,
      roomId: message.roomId,
      code: JoinErrorCode.Unknown,
      message: "unsupported crdt"
    });
  }

  private async handleDocUpdate(ws: WebSocket, state: SocketState, message: DocUpdate): Promise<void> {
    if (message.updates.some((u) => u.length > MAX_MESSAGE_SIZE)) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge);
      return;
    }
    if (!state.rooms.includes(message.crdt) && !(message.crdt === CrdtType.Loro && state.loroRecoveryRoomId === message.roomId)) {
      // A write from a socket that never (re)joined: PermissionDenied makes
      // the client rejoin, but the condition itself is the `rooms: []`
      // broadcast-exclusion state — log who hit it.
      console.warn(
        "update from non-member socket",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state.deviceId ?? "unattributed"}`,
        `crdt=${message.crdt}`
      );
      this.ack(ws, message, UpdateStatusCode.PermissionDenied);
      return;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, message.updates);
  }

  /** Shared apply path for whole and reassembled updates. */
  private async applyUpdates(
    ws: WebSocket,
    state: SocketState,
    crdt: CrdtType,
    roomId: string,
    batchId: `0x${string}`,
    updates: Uint8Array[]
  ): Promise<void> {
    if (
      crdt === CrdtType.Loro &&
      !canPublish(state.capabilities)
    ) {
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.PermissionDenied, batchId);
      return;
    }
    if (crdt === CrdtType.Loro) {
      await this.ensureDoc();
      if (!(await this.authorizeSocket(ws, state))) return;
      // A reset closes the old socket even if another request has already
      // materialized a replacement. Never admit that socket's in-flight write.
      if (ws.readyState !== WebSocket.OPEN) return;
      // Authority lookup may yield to a trim (which frees/replaces the doc)
      // or idle release. Use only the current live doc, with no further await.
      let doc = this.doc;
      if (!doc) {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.workspace) this.notifyWorkspace(doc, state.projectScope, true);
      try {
        if (state.loroRecoveryRoomId !== undefined) {
          if (state.loroRecoveryRoomId !== roomId || updates.length !== 1 || updates[0].length === 0) {
            throw new Error("recovery requires a complete snapshot");
          }
          const metadata = decodeImportBlobMeta(updates[0], false);
          try {
            if (metadata.mode !== "snapshot" && metadata.mode !== "shallow-snapshot" && metadata.mode !== "outdated-snapshot") {
              throw new Error("recovery requires a complete snapshot");
            }
          } finally {
            metadata.partialStartVersionVector.free();
            metadata.partialEndVersionVector.free();
          }
        }
        doc = this.importLoroUpdates(doc, updates);
      } catch (error) {
        this.escalateWasmPoisoning(error);
        // Missing dependencies or an irreversible shallow-history gap require
        // peer recovery; never acknowledge a merely pending import as applied.
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.workspace) this.notifyWorkspace(doc, state.projectScope);
      await this.ctx.storage.sync();
      // The grant may expire or be revoked while durable persistence yields.
      if (!(await this.authorizeSocket(ws, state)) || ws.readyState !== WebSocket.OPEN) return;
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      await this.relay(ws, crdt, roomId, updates);
      return;
    }
    if (crdt === CrdtType.LoroEphemeralStore) {
      if (updates.some((update) => update.length > MAX_PRESENCE_UPDATE_BYTES)) {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.PayloadTooLarge, batchId);
        return;
      }
      if (state.workspace) {
        this.cacheWorkspacePresence(state.deviceId, updates, Date.now());
      }
      if (!state.workspace) {
        const eph = this.ensureEph();
        try {
          for (const update of updates) if (update.length > 0) eph.apply(update);
        } catch {
          this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
          return;
        }
      }
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      await this.relay(ws, crdt, roomId, updates);
      return;
    }
    this.ack(ws, { crdt, roomId }, UpdateStatusCode.Unknown, batchId);
  }

  private importLoroUpdates(doc: LoroDoc, updates: Uint8Array[]): LoroDoc {
    if (updates.every((update) => update.length === 0)) {
      this.assertLoroMaterialized(doc, this.docPendingEnds);
      return doc;
    }
    const snapshotIndex = updates.findIndex((update) => {
      if (update.length === 0) return false;
      const metadata = decodeImportBlobMeta(update, false);
      try {
        return metadata.mode === "snapshot" || metadata.mode === "shallow-snapshot" || metadata.mode === "outdated-snapshot";
      } finally {
        metadata.partialStartVersionVector.free();
        metadata.partialEndVersionVector.free();
      }
    });
    if (snapshotIndex < 0) {
      // Reject a whole batch atomically. Only fully materialized deltas enter SQL.
      // Reconstruct accepted state from its durable baseline only on failure.
      const before = doc.oplogFrontiers();
      const retainedPending = this.docPendingEnds;
      let pendingEnds = retainedPending ? new Map(retainedPending) : undefined;
      try {
        for (const update of updates) {
          if (update.length === 0) continue;
          const imported = doc.import(update);
          for (const [peer, span] of imported.pending ?? []) {
            pendingEnds ??= new Map();
            pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
          }
        }
        this.assertLoroMaterialized(doc, pendingEnds);
        const after = doc.oplogFrontiers();
        const changed = before.length !== after.length ||
          !before.every((head) => after.some((other) => head.peer === other.peer && head.counter === other.counter));
        if (changed || retainedPending?.size) {
          this.recordLoroUpdates(doc, updates, Boolean(pendingEnds?.size));
        }
        this.docPendingEnds = undefined;
        return doc;
      } catch (error) {
        this.doc = undefined;
        doc.free();
        let restored: LoroDoc | undefined;
        try {
          restored = new LoroDoc();
          const baseline = this.blobs.get("snapshot");
          if (baseline?.length) restored.import(baseline);
          const pendingEnds = this.replayLog(restored).pendingEnds;
          this.assertLoroMaterialized(restored, pendingEnds);
          this.doc = restored;
          this.docPendingEnds = retainedPending;
        } catch (rollbackError) {
          this.doc = undefined;
          restored?.free();
          throw new AggregateError([error, rollbackError], "Loro update rollback failed; accepted bytes retained");
        }
        throw error;
      }
    }
    // Snapshot bootstraps need an isolated candidate: import state before any
    // retained deltas to keep the lazy snapshot path and preserve both branches.
    let candidate = new LoroDoc();
    let ownsCandidate = true;
    let previousVersion: VersionVector | undefined;
    let pendingEnds: Map<PeerID, number> | undefined;
    const replay = (update: Uint8Array): void => {
      if (update.length === 0) return;
      const imported = candidate.import(update);
      for (const [peer, span] of imported.pending ?? []) {
        pendingEnds ??= new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
      }
    };
    try {
      previousVersion = doc.oplogVersion();
      let baseline = updates[snapshotIndex];
      if (baseline?.length) replay(baseline);
      if (snapshotIndex >= 0) {
        this.assertLoroMaterialized(candidate, pendingEnds);
        const candidateVersion = candidate.oplogVersion();
        try {
          const coverage = candidateVersion.compare(previousVersion);
          if (coverage !== undefined && coverage <= 0) {
            // Covered snapshots add nothing; do not replay them into warm state.
            candidate.free();
            ownsCandidate = false;
            return this.importLoroUpdates(doc, updates.filter((_, i) => i !== snapshotIndex));
          }
          let preserveRetainedHistory = false;
          if (this.retainsWorkspaceHistory() && previousVersion.length() > 0) {
            const retainedSince = doc.shallowSinceVV();
            const incomingSince = candidate.shallowSinceVV();
            try {
              const historyCoverage = incomingSince.compare(retainedSince);
              preserveRetainedHistory = historyCoverage === undefined || historyCoverage > 0;
            } finally { incomingSince.free(); retainedSince.free(); }
          }
          if (preserveRetainedHistory) {
            // A client's shallow snapshot must not trim the workspace history
            // that disconnected writers still need, even when its VV is newer.
            const incoming = candidate;
            candidate = doc.fork();
            incoming.free();
            replay(baseline);
          } else if (coverage === undefined) {
            const retainedSince = doc.shallowSinceVV();
            try {
              const retainedCoverage = candidateVersion.compare(retainedSince);
              if (retainedCoverage === undefined || retainedCoverage < 0) {
                throw new Error("snapshot does not cover retained-history boundary");
              }
            } finally { retainedSince.free(); }
            // Preserve unique operations already folded into the old baseline.
            // Exporting a delta is lossless only above its shallow boundary.
            replay(doc.export({ mode: "update", from: candidateVersion }));
          }
        } finally { candidateVersion.free(); }
      }
      if (this.docPendingEnds?.size) {
        const retained = this.replayLog(candidate).pendingEnds;
        for (const [peer, end] of this.docPendingEnds) {
          pendingEnds ??= new Map();
          pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
        }
        for (const [peer, end] of retained ?? []) {
          pendingEnds ??= new Map();
          pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
        }
      }
      for (let i = 0; i < updates.length; i++) {
        if (i !== snapshotIndex) replay(updates[i]);
      }
      this.assertLoroMaterialized(candidate, pendingEnds);
      const mergedVersion = candidate.oplogVersion();
      try {
        const coverage = mergedVersion.compare(previousVersion);
        if (coverage === undefined || coverage < 0) throw new Error("import lost retained operations");
      } finally { mergedVersion.free(); }
      // Persist the validated union, never the publisher's partial baseline.
      // Replacement and log deletion share a synchronous transaction, so writes
      // accepted while storage.sync yields stay in the new log.
      this.recordLoroUpdates(candidate, updates, true);
      this.doc = candidate;
      this.docPendingEnds = undefined;
      ownsCandidate = false;
      doc.free();
      return candidate;
    } finally {
      previousVersion?.free();
      if (ownsCandidate) candidate.free();
    }
  }

  private assertLoroMaterialized(doc: LoroDoc, pendingEnds?: Map<PeerID, number>): void {
    if (pendingEnds?.size) {
      const version = doc.oplogVersion();
      try {
        for (const [peer, end] of pendingEnds) {
          if ((version.get(peer) ?? 0) < end) throw new Error("import has unresolved dependencies");
        }
      } finally { version.free(); }
    }
    // Frontiers avoid walking the entire causal DAG for a lazy snapshot.
    const state = doc.frontiers();
    const oplog = doc.oplogFrontiers();
    if (doc.isDetached() || state.length !== oplog.length ||
        !state.every((head) => oplog.some((other) => head.peer === other.peer && head.counter === other.counter))) {
      throw new Error("import did not materialize complete state");
    }
  }

  private workspaceNotifications(): WorkspaceNotifications {
    return this.notifications ??= new WorkspaceNotifications(this.ctx.storage, this.env);
  }

  private notifyWorkspace(doc: LoroDoc, projectScope: string, baseline = false): void {
    let phase = "workspace_notifications_construct";
    try {
      const notifications = this.workspaceNotifications();
      phase = "observe";
      const events = notifications.observe(doc, baseline);
      phase = "schedule_delivery";
      if (events.length) {
        this.ctx.waitUntil(notifications.deliver(events, projectScope).catch(() => {
          console.warn("Crew push delivery failed");
        }));
      }
    } catch (error) {
      if (this.env.ENVIRONMENT === "staging") console.warn("Crew push observation failed", phase, (error instanceof Error ? error.message : String(error)).slice(0, 512));
      else console.warn("Crew push observation failed");
    }
  }

  /** Commit accepted bytes and fold status churn without discarding CRDT history.
   * No accepted payload remains in a volatile JS buffer after this returns. */
  private recordLoroUpdates(doc: LoroDoc, updates: Uint8Array[], forceSnapshot = false): void {
    const bytes = updates.reduce((total, update) => total + update.byteLength, 0);
    if (!bytes) return;
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + bytes;
    const rows = Number([...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n ?? 0);
    const fold = forceSnapshot || logBytes >= COMPACT_LOG_BYTES ||
      rows + updates.length >= COMPACT_LOG_ROWS || updates.some((update) => update.byteLength > CHUNK_BYTES);
    // Export before mutating SQL: a WASM failure cannot partially admit a batch.
    const snapshot = fold ? doc.export({ mode: "snapshot" }) : undefined;
    this.ctx.storage.transactionSync(() => {
      if (snapshot) {
        this.blobs.put("snapshot", snapshot);
        this.ctx.storage.sql.exec("DELETE FROM updates");
        this.setMeta("updateBytes", "0");
      } else {
        const now = Date.now();
        for (const update of updates) {
          if (!update.byteLength) continue;
          const value = update.byteOffset === 0 && update.byteLength === update.buffer.byteLength
            ? update.buffer : update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength);
          this.ctx.storage.sql.exec("INSERT INTO updates (bytes, received_at) VALUES (?, ?)", value, now);
        }
        this.setMeta("updateBytes", String(logBytes));
      }
      this.setMeta("tailDirty", "1");
      this.setMeta("backupDirty", "1");
      this.setMeta("postReset", "0");
    });
    this.markActivity();
  }

  private pruneFragments(now: number): void {
    for (const [socket, batches] of this.fragments) {
      for (const [id, batch] of batches) if (batch.expiresAt <= now) batches.delete(id);
      if (!batches.size) this.fragments.delete(socket);
    }
    clearTimeout(this.fragmentTimer);
    this.fragmentTimer = undefined;
    if (this.fragments.size) {
      const expiry = Math.min(...[...this.fragments.values()].flatMap((batches) => [...batches.values()].map((batch) => batch.expiresAt)));
      this.fragmentTimer = setTimeout(() => this.pruneFragments(Date.now()), Math.max(1, expiry - now));
    }
  }

  private handleFragmentHeader(
    ws: WebSocket,
    state: SocketState,
    message: DocUpdateFragmentHeader
  ): void {
    if (!state.rooms.includes(message.crdt) && !(message.crdt === CrdtType.Loro && state.loroRecoveryRoomId === message.roomId)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    this.pruneFragments(Date.now());
    let batches = this.fragments.get(ws);
    if (
      !Number.isSafeInteger(message.fragmentCount) ||
      message.fragmentCount <= 0 ||
      message.fragmentCount > MAX_FRAGMENT_COUNT ||
      !Number.isSafeInteger(message.totalSizeBytes) ||
      message.totalSizeBytes < 0 ||
      message.totalSizeBytes > MAX_REASSEMBLED_BYTES
    ) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    let reservedBytes = message.totalSizeBytes;
    let reservedParts = message.fragmentCount;
    // The WASM heap is isolate-shared: a per-socket budget still admits N*64MiB.
    for (const [socket, inFlight] of this.fragments) {
      for (const [id, batch] of inFlight) {
        if (socket === ws && id === message.batchId) continue;
        reservedBytes += batch.totalSize;
        reservedParts += batch.parts.length;
      }
    }
    if (reservedBytes > MAX_REASSEMBLED_BYTES || reservedParts > MAX_FRAGMENT_COUNT) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    if (!batches) {
      batches = new Map();
      this.fragments.set(ws, batches);
    }
    batches.set(message.batchId, {
      parts: new Array<Uint8Array | undefined>(message.fragmentCount),
      received: 0,
      receivedBytes: 0,
      totalSize: message.totalSizeBytes,
      header: message,
      expiresAt: Date.now() + FRAGMENT_TTL_MS
    });
    this.pruneFragments(Date.now());
  }

  private async handleFragment(
    ws: WebSocket,
    state: SocketState,
    message: { crdt: CrdtType; roomId: string; batchId: `0x${string}`; index: number; fragment: Uint8Array }
  ): Promise<void> {
    this.pruneFragments(Date.now());
    const batch = this.fragments.get(ws)?.get(message.batchId);
    if (!batch) {
      // Unknown batch (e.g. header lost to hibernation) — tell the sender to
      // retry the whole batch.
      this.ack(ws, message, UpdateStatusCode.FragmentTimeout, message.batchId);
      return;
    }
    if (
      message.crdt !== batch.header.crdt ||
      message.roomId !== batch.header.roomId ||
      !Number.isSafeInteger(message.index) ||
      message.index < 0 ||
      message.index >= batch.parts.length
    ) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
      return;
    }
    // Retransmission must not count a part twice or complete a sparse batch.
    if (batch.parts[message.index] !== undefined) return;
    if (message.fragment.length > batch.totalSize - batch.receivedBytes) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    // Retain only fragment bytes, not the larger decoded frame's backing buffer.
    batch.parts[message.index] = message.fragment.slice();
    batch.received++;
    batch.receivedBytes += message.fragment.length;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    if (batch.receivedBytes !== batch.totalSize) {
      this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
      return;
    }
    const total = new Uint8Array(batch.totalSize);
    let off = 0;
    for (let i = 0; i < batch.parts.length; i++) {
      const part = batch.parts[i];
      if (part === undefined) throw new Error("incomplete fragment batch");
      total.set(part, off);
      off += part.length;
      batch.parts[i] = undefined;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
  }

  // ── doc/ephemeral materialization ────────────────────────────────────────

  private async ensureDoc(): Promise<LoroDoc> {
    this.touchDoc();
    if (this.docLoad) return this.docLoad;
    if (this.doc) return this.doc;
    this.docLoad = this.materializeDoc();
    try {
      return await this.docLoad;
    } finally {
      this.docLoad = undefined;
    }
  }
  /** Materialize the complete journal union after bootstrapping its snapshot. */
  private replayLog(doc: LoroDoc): { rows: number; batches: number; pendingEnds?: Map<PeerID, number> } {
    const updates: Uint8Array[] = [];
    for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
      updates.push(new Uint8Array(row.bytes as ArrayBuffer));
    }
    if (!updates.length) return { rows: 0, batches: 0 };
    const imported = doc.importBatch(updates);
    let pendingEnds: Map<PeerID, number> | undefined;
    for (const [peer, span] of imported.pending ?? []) {
      pendingEnds ??= new Map();
      pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
    }
    return { rows: updates.length, batches: 1, pendingEnds };
  }

  private async materializeDoc(): Promise<LoroDoc> {
    // A crash counter is telemetry, not evidence that accepted workspace bytes
    // are disposable or that a subsequent complete replay cannot heal the room.
    let attempts = Number(this.getMeta("replayAttempts") ?? "0");
    // Replay failures must not permanently lock a workspace, even after a
    // transient resource failure or a deployment that fixes replay itself.
    if (attempts >= REPLAY_CRASH_LIMIT && !this.retainsWorkspaceHistory()) {
      this.dropLog();
      // Boot every attached socket, exactly like POST /reset-log. The
      // automated wedge break used to swap the doc out from UNDER live
      // sessions: their next writes carried deps the emptied doc lacks,
      // imports failed, clients burned their capped invalid-rejoin resyncs
      // and then sat LATCHED — rows frozen on a healthy-looking socket
      // (2026-08-04: work-metal's workspace status never updated again
      // after the 20:16Z wedge-break while its chat rooms streamed fine).
      // A close → redial → empty-VV join re-uploads full state instead.
      this.closeSocketsForRoomReset();
      attempts = 0;
    }
    this.setMeta("replayAttempts", String(attempts + 1));
    // Persist telemetry before crossing WASM so resource deaths remain visible.
    await this.ctx.storage.sync();
    const started = Date.now();
    const doc = new LoroDoc();
    const snapshot = this.blobs.get("snapshot");
    let pendingEnds: Map<PeerID, number> | undefined;
    const replay = (bytes: Uint8Array | Uint8Array[]): void => {
      const imported = Array.isArray(bytes) ? doc.importBatch(bytes) : doc.import(bytes);
      for (const [peer, span] of imported.pending ?? []) {
        pendingEnds ??= new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
      }
    };
    // Bootstrap the snapshot separately: batching it with dependent deltas
    // can leave shallow snapshots unresolved in Loro 1.13.9.
    if (snapshot?.length) {
      try {
        replay(snapshot);
      } catch (error) {
        return await this.rejectPersistedLoroState(doc, "snapshot", error);
      }
    }
    let rows = 0;
    let batches = 0;
    try {
      const replayed = this.replayLog(doc);
      rows = replayed.rows;
      batches = replayed.batches;
      for (const [peer, end] of replayed.pendingEnds ?? []) {
        pendingEnds ??= new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
      }
    } catch (error) {
      return await this.rejectPersistedLoroState(doc, "update journal", error);
    }
    this.setMeta("replayAttempts", "0");
    // Reset only after a successful replay; later export failures never reset
    // accepted workspace state. Both old and current clients keep their history.
    await this.ctx.storage.sync();
    // Cold-start telemetry (Workers Logs + /stats): the replay cost is the
    // wedge risk — watch lastReplayMs trend toward the CPU limit to catch the
    // next 2026-07-30 while it is still a statistic, not an incident.
    const replayMs = Date.now() - started;
    this.setMeta("lastReplayMs", String(replayMs));
    this.setMeta("lastReplayRows", String(rows));
    this.setMeta("lastReplayBatches", String(batches));
    console.log(
      `cold replay: ${replayMs}ms, ${rows} rows, snapshot ${snapshot?.length ?? 0}B, attempt ${attempts + 1}`,
      `room=${this.getMeta("chatId") ?? "?"}`
    );
    this.doc = doc;
    if (pendingEnds?.size) {
      const version = doc.oplogVersion();
      try {
        for (const [peer, end] of pendingEnds) {
          if ((version.get(peer) ?? 0) >= end) pendingEnds.delete(peer);
        }
      } finally { version.free(); }
    }
    this.docPendingEnds = pendingEnds?.size ? pendingEnds : undefined;
    // Migrate long legacy journals once. Never fold unresolved accepted history:
    // only a peer carrying its missing dependencies can complete that baseline.
    if (rows >= COMPACT_LOG_ROWS && !this.docPendingEnds) {
      this.assertLoroMaterialized(doc);
      const baseline = doc.export({ mode: "snapshot" });
      this.ctx.storage.transactionSync(() => {
        this.blobs.put("snapshot", baseline);
        this.ctx.storage.sql.exec("DELETE FROM updates");
        this.setMeta("updateBytes", "0");
      });
      await this.ctx.storage.sync();
    }
    // Record a frontier checkpoint on cold start too: the alarm only records
    // while WRITES keep it armed, so an idle room never aged into trim
    // eligibility — it could never shrink, ever. One checkpoint a day max.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const newest = checkpoints[checkpoints.length - 1];
    if (!newest || Date.now() - newest.at >= DAY_MS) {
      checkpoints.push({
        at: Date.now(),
        frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
      });
      while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();
      this.setMeta("checkpoints", JSON.stringify(checkpoints));
    }
    // Trim on cold materialization too: fold and alarm both ride WRITES, so
    // an idle-but-watched room NEVER trimmed — yet every isolate restart
    // re-materializes its full history into the shared wasm heap (the
    // 2026-08-04 exhaustion recurred post-fix on exactly those rooms). The
    // one-off export cost here permanently shrinks the room.
    if (await this.trimHistoryIfDue(doc, Date.now())) {
      console.log(`history trimmed on cold start room=${this.getMeta("chatId") ?? "?"}`);
    }
    this.setMeta("lastColdMs", String(Date.now() - started));
    return this.doc;
  }

  /** Free idle WASM handles; accepted history already lives in SQL. */
  private touchDoc(): void {
    this.lastDocUse = Date.now();
    if (this.docIdleTimer) return;
    this.docIdleTimer = setTimeout(() => this.releaseIdleDoc(), DOC_IDLE_RELEASE_MS + 500);
  }

  private releaseIdleDoc(): void {
    this.docIdleTimer = undefined;
    if (!this.doc && !this.eph) return;
    const idle = Date.now() - this.lastDocUse;
    if (idle < DOC_IDLE_RELEASE_MS || this.docLoad) {
      this.docIdleTimer = setTimeout(
        () => this.releaseIdleDoc(),
        Math.max(DOC_IDLE_RELEASE_MS - idle, 1_000) + 500
      );
      return;
    }
    this.doc?.free();
    this.doc = undefined;
    this.eph?.free();
    this.eph = undefined;
    this.workspacePresence.clear();
  }

  private closeSocketsForRoomReset(): void {
    for (const sock of this.ctx.getWebSockets()) {
      try {
        sock.close(4410, "room reset");
      } catch {
        /* already gone */
      }
    }
  }

  /** Never erase workspace/unknown history on a failed replay. Session rooms
   * retain their existing reset policy; explicit administrator resets remain
   * separate from automatic recovery. */
  private async rejectPersistedLoroState(
    doc: LoroDoc,
    source: string,
    error: unknown
  ): Promise<never> {
    console.error(
      "persisted Loro replay rejected",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `source=${source}`,
      String(error)
    );
    try {
      doc.free();
    } catch {
      /* a wasm panic may already have invalidated the handle */
    }
    this.doc = undefined;
    if (!this.retainsWorkspaceHistory()) this.dropLog();
    await this.ctx.storage.sync();
    this.closeSocketsForRoomReset();
    throw error;
  }

  /** Drop the persisted update log + snapshot (the /reset-log storage clear):
   * the next materialization starts empty and engines re-upload state on
   * rejoin. Preserves owner/chatId meta. */
  private dropLog(): void {
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.blobs.delete("snapshot");
    this.setMeta("updateBytes", "0");
    this.setMeta("checkpoints", "[]");
    this.setMeta("lastTrimAt", "");
    this.docPendingEnds = undefined;
    // Until an engine re-uploads real state, anything materialized from here
    // is empty — postReset gates the nightly R2 put so the DISASTER backup
    // cannot be overwritten by the emptied doc. Without it, the durable crash
    // counter let alarm auto-retries complete a wedge break with ZERO clients
    // connected and then back up the empty doc in the same invocation,
    // destroying the one copy that exists for the engine-never-returns case
    // (adversarial-review finding). Cleared by recordLoroUpdates.
    this.setMeta("postReset", "1");
    this.setMeta("replayAttempts", "0");
  }

  private ensureEph(): EphemeralStore {
    this.touchDoc();
    if (!this.eph) this.eph = new EphemeralStore(30_000);
    return this.eph;
  }

  private pruneWorkspacePresence(now: number) {
    for (const [deviceId, entry] of this.workspacePresence) {
      if (entry.expiresAt <= now) this.workspacePresence.delete(deviceId);
    }
  }

  private workspacePresenceSnapshot(deviceId: string | undefined, now: number) {
    this.pruneWorkspacePresence(now);
    const updates: Uint8Array[] = [];
    for (const [peerDeviceId, entry] of this.workspacePresence) {
      if (peerDeviceId !== deviceId) updates.push(...entry.updates);
    }
    return updates;
  }

  private cacheWorkspacePresence(
    deviceId: string | undefined,
    updates: Uint8Array[],
    now: number
  ) {
    if (!deviceId) return;
    const totalBytes = updates.reduce((total, update) => total + update.byteLength, 0);
    if (totalBytes === 0 || totalBytes > MAX_PRESENCE_UPDATE_BYTES) return;
    this.pruneWorkspacePresence(now);
    this.workspacePresence.delete(deviceId);
    while (this.workspacePresence.size >= MAX_WORKSPACE_PRESENCE_PEERS) {
      const oldest = this.workspacePresence.keys().next().value;
      if (oldest === undefined) break;
      this.workspacePresence.delete(oldest);
    }
    this.workspacePresence.set(deviceId, {
      expiresAt: now + WORKSPACE_PRESENCE_TTL_MS,
      updates: updates.map((update) => update.slice())
    });
  }

  // ── durability: flush, compaction, backups ───────────────────────────────

  private async flush(): Promise<void> {
    // Admission already persisted the log. Retain session-only trimming on
    // maintenance reads without putting workspace exports on every stats read.
    if (!this.retainsWorkspaceHistory() && (this.blobs.byteLength("snapshot") ?? 0) > TRIM_FORCE_BYTES) {
      await this.foldLog();
    }
    await this.ctx.storage.sync();
  }

  /** Maintenance fold. Workspace folds are lossless; session trimming is unchanged. */
  private async foldLog(): Promise<void> {
    await this.ensureDoc();
    let doc = this.doc;
    if (!doc) throw new Error("document released during log fold");
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    if (await this.trimHistoryIfDue(doc, Date.now())) return;
    // Even a no-op async trim yields; an accepted snapshot may replace/free it.
    await this.ensureDoc();
    doc = this.doc;
    if (!doc) throw new Error("document released during log fold");
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    const snapshot = doc.export({ mode: "snapshot" });
    this.ctx.storage.transactionSync(() => {
      this.blobs.put("snapshot", snapshot);
      this.ctx.storage.sql.exec("DELETE FROM updates");
      this.setMeta("updateBytes", "0");
    });
    await this.ctx.storage.sync();
  }

  private retainsWorkspaceHistory(): boolean {
    const kind = this.getMeta("roomKind");
    return kind === "workspace" || (kind !== "session" && !canonicalSessionId(this.getMeta("chatId")));
  }

  /** HISTORY TRIM (§3.1): shallow snapshot at the newest recorded frontier
   * checkpoint older than RETAIN_DAYS — history before it is discarded
   * permanently, state fully preserved. Returns whether a trim landed (the
   * snapshot + log + materialized doc were all replaced — the passed `doc`
   * is CONSUMED: its wasm memory is freed, callers must switch to
   * `this.doc`). Best-effort: any export failure leaves the room to the
   * caller's lossless fold. */
  private async trimHistoryIfDue(doc: LoroDoc, now: number): Promise<boolean> {
    // Offline workspace writers may still depend on any accepted operation.
    // Pending dependencies also mean the materialized state is incomplete.
    if (this.retainsWorkspaceHistory() || this.docPendingEnds?.size) return false;
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const cutoff = checkpoints.filter((c) => now - c.at >= RETAIN_MS).pop();
    let frontiers: { peer: `${number}`; counter: number }[];
    // The durable lastTrimAt marker identifies the cutoff already applied.
    // A cold start or regular snapshot re-export must not re-trim that cutoff,
    // regardless of the materialized document's shallow status.
    const retainedBytes = (this.blobs.byteLength("snapshot") ?? 0) +
      Number(this.getMeta("updateBytes") ?? "0");
    if (cutoff && this.getMeta("lastTrimAt") !== String(cutoff.at)) {
      frontiers = cutoff.frontiers.map((f) => ({ peer: f.peer as `${number}`, counter: f.counter }));
    } else if (retainedBytes > TRIM_FORCE_BYTES) {
      // No aged checkpoint but the full history is already a heap hazard:
      // trim at the current frontier (see TRIM_FORCE_BYTES).
      frontiers = doc.frontiers().map((f) => ({ peer: String(f.peer) as `${number}`, counter: f.counter }));
    } else {
      return false;
    }
    try {
      const shallow = doc.export({
        mode: "shallow-snapshot",
        frontiers
      });
      const fresh = new LoroDoc();
      try {
        fresh.import(shallow);
        this.assertLoroMaterialized(fresh);
        this.ctx.storage.transactionSync(() => {
          this.blobs.put("snapshot", shallow);
          this.ctx.storage.sql.exec("DELETE FROM updates");
          this.setMeta("updateBytes", "0");
          this.setMeta("lastTrimAt", String(cutoff?.at ?? now));
        });
      } catch (error) {
        fresh.free();
        throw error;
      }
      this.doc = fresh;
      // Free the replaced full-history doc NOW — waiting on GC finalizers
      // leaks it into the shared wasm heap exactly when trimming was
      // supposed to relieve it (see handleJoin).
      if (doc !== fresh) doc.free();
    } catch (error) {
      console.error("history trim failed", `room=${this.getMeta("chatId") ?? "?"}`, String(error));
      return false;
    }
    // The live document is already replaced before yielding, so accepted writes
    // cannot land in the discarded pre-trim doc. A durability failure must
    // propagate, not fall back to exporting that now-freed document.
    await this.ctx.storage.sync();
    return true;
  }

  private closeExpiredGrantSockets(now: number): void {
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment() as SocketState | null;
      if (
        !Number.isSafeInteger(state?.grantExpiresAt) ||
        (state?.grantExpiresAt as number) > now
      ) {
        continue;
      }
      try {
        ws.close(4403, "device grant expired");
      } catch {
        /* already gone */
      }
    }
  }

  private async scheduleGrantExpiryAlarm(): Promise<void> {
    const now = Date.now();
    let earliest: number | undefined;
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment() as SocketState | null;
      const expiresAt = state?.grantExpiresAt;
      if (!Number.isSafeInteger(expiresAt) || (expiresAt as number) <= now) continue;
      if (earliest === undefined || (expiresAt as number) < earliest) {
        earliest = expiresAt as number;
      }
    }
    if (earliest === undefined) return;
    const scheduled = await this.ctx.storage.getAlarm();
    if (scheduled === null || earliest < scheduled) {
      await this.ctx.storage.setAlarm(earliest);
    }
  }

  /** Daily maintenance and device-grant expiry alarm. */
  async alarm(): Promise<void> {
    const now = Date.now();
    this.closeExpiredGrantSockets(now);
    await this.flush();
    if (this.getMeta("backupDirty") !== "1") {
      await this.scheduleGrantExpiryAlarm();
      return;
    }
    await this.ensureDoc();
    const doc = this.doc;
    if (!doc) throw new Error("document released during maintenance");

    // 1. Record today's frontier checkpoint.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    checkpoints.push({
      at: now,
      frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
    });
    while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();

    // 2. HISTORY TRIM — must see today's checkpoint list, so persist first.
    //    (Also fires from foldLog, which is what usually gets there first on
    //    a high-churn room.)
    this.setMeta("checkpoints", JSON.stringify(checkpoints));
    await this.trimHistoryIfDue(doc, now);
    await this.ensureDoc();

    // 3. Nightly R2 backup (§3.3) — full current snapshot, disaster hatch.
    // Two guards (round-2 review): postReset pauses the put between a
    // wedge-break drop and the first re-uploaded state, and the put is
    // MONOTONIC — the new snapshot must version-include the previously
    // backed-up one, so even a post-drop doc that took a few fresh writes
    // (clearing postReset) can never replace the last good copy with a
    // hollow one. CRDT merge guarantees a genuinely recovered doc includes
    // the old VV, at which point the put resumes; until then backupDirty
    // stays set and the alarm chain keeps trying.
    const chatId = this.getMeta("chatId");
    if (chatId && this.getMeta("postReset") !== "1" && !this.docPendingEnds?.size) {
      const current = this.doc;
      if (!current) throw new Error("document released during backup");
      this.assertLoroMaterialized(current, this.docPendingEnds);
      const prevVV = this.getMeta("backupVV");
      let advances = true;
      if (prevVV) {
        let prev: VersionVector | undefined;
        let cur: VersionVector | undefined;
        try {
          prev = VersionVector.decode(Uint8Array.from(atob(prevVV), (c) => c.charCodeAt(0)));
          cur = current.oplogVersion();
          const cmp = cur.compare(prev);
          advances = cmp !== undefined && cmp >= 0;
        } catch {
          /* unreadable meta: allow the put and rewrite it below */
        } finally {
          // Explicit frees: see handleJoin — GC finalizers don't run under
          // wasm-side memory pressure.
          prev?.free();
          cur?.free();
        }
      }
      if (advances) {
        const snapshot = current.export({ mode: "snapshot" });
        const vv = current.oplogVersion();
        try {
          const encodedVersion = btoa(String.fromCharCode(...vv.encode()));
          await this.env.BLOBS.put(`backup/${chatId}/latest.loro`, snapshot);
          // The original doc may have been freed, or advanced in place, while
          // R2 persisted this snapshot. Its metadata must describe these bytes.
          this.setMeta("backupVV", encodedVersion);
          const liveVersion = this.doc?.oplogVersion();
          try {
            if (liveVersion?.compare(vv) === 0 && !this.docPendingEnds?.size) {
              this.setMeta("backupDirty", "0");
            }
          } finally { liveVersion?.free(); }
        } finally { vv.free(); }
      }
    }
    // Re-arm only while there is a reason to wake again; markActivity re-arms
    // on the next write otherwise.
    await this.scheduleGrantExpiryAlarm();
  }

  /** Arm the daily alarm if none is scheduled (called on every write). */
  private markActivity(): void {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  private async currentTail(): Promise<unknown> {
    await this.flush();
    if (this.getMeta("tailDirty") !== "1") {
      const cached = getJsonBlob<unknown>(this.blobs, "tail");
      if (cached !== undefined) return cached;
    }
    const doc = await this.ensureDoc();
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    const tail = materializeTail(doc, Date.now());
    putJsonBlob(this.blobs, "tail", tail);
    this.setMeta("tailDirty", "0");
    return tail;
  }

  // ── wire helpers ─────────────────────────────────────────────────────────

  /** Returns false when the frame could not be delivered (socket gone /
   * runtime refused the send). Encode failures throw out instead — they are
   * OUR bug, never the peer's, and must not be mistaken for a deaf socket. */
  private send(ws: WebSocket, message: ProtocolMessage): boolean {
    const bytes = encode(message);
    try {
      ws.send(bytes);
      return true;
    } catch {
      /* socket already gone; hibernation API cleans it up */
      return false;
    }
  }

  /** Send updates, fragmenting any single update above FRAGMENT_BYTES and
   * chunking small ones so no encoded frame approaches the loro-protocol
   * 256KB message cap (envelope overhead included). Returns false if any
   * frame failed to deliver. */
  private sendUpdates(ws: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): boolean {
    let ok = true;
    let batch: Uint8Array[] = [];
    let batchBytes = 0;
    const flushBatch = () => {
      if (batch.length === 0) return;
      ok =
        this.send(ws, {
          type: MessageType.DocUpdate,
          crdt,
          roomId,
          updates: batch,
          batchId: this.newBatchId()
        }) && ok;
      batch = [];
      batchBytes = 0;
    };
    for (const update of updates) {
      if (update.length <= FRAGMENT_BYTES) {
        if (batchBytes + update.length > FRAGMENT_BYTES) flushBatch();
        batch.push(update);
        batchBytes += update.length;
        continue;
      }
      // Never move small dependent deltas ahead of a fragmented baseline.
      flushBatch();
      const batchId = this.newBatchId();
      const fragmentCount = Math.ceil(update.length / FRAGMENT_BYTES);
      ok =
        this.send(ws, {
          type: MessageType.DocUpdateFragmentHeader,
          crdt,
          roomId,
          batchId,
          fragmentCount,
          totalSizeBytes: update.length
        }) && ok;
      for (let i = 0; i < fragmentCount; i++) {
        ok =
          this.send(ws, {
            type: MessageType.DocUpdateFragment,
            crdt,
            roomId,
            batchId,
            index: i,
            fragment: update.subarray(
              i * FRAGMENT_BYTES,
              Math.min((i + 1) * FRAGMENT_BYTES, update.length)
            )
          }) && ok;
      }
    }
    flushBatch();
    return ok;
  }

  /** Relay accepted updates to every other member socket via sendUpdates —
   * NOT a single pre-encoded frame. broadcast() used to encode the batch
   * once, so a reassembled >256KB client push (a device re-uploading its
   * full workspace history after a server reset) blew the loro-protocol
   * message cap and NEVER reached peers live; they only converged via a
   * later rejoin backfill (2026-08-04, the last silent-staleness path). */
  private async relay(
    from: WebSocket,
    crdt: CrdtType,
    roomId: string,
    updates: Uint8Array[]
  ): Promise<void> {
    for (const ws of this.ctx.getWebSockets()) {
      if (ws === from) continue;
      const state = ws.deserializeAttachment() as SocketState | null;
      if (!state?.rooms.includes(crdt)) continue;
      if (!(await this.authorizeSocket(ws, state))) continue;
      if (!this.sendUpdates(ws, crdt, roomId, updates)) {
        // A member socket we cannot send to is a DEAF PEER, not a skippable
        // one: swallowing the failure left it looking alive (runtime
        // auto-pongs, accepted writes) while it silently missed every
        // broadcast until an app restart (2026-08-04 incident). Close it so
        // the client's session ends and its redial + VV backfill heal the
        // gap within seconds.
        console.warn(
          "relay send failed; closing socket",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `device=${state.deviceId ?? "unattributed"}`
        );
        try {
          ws.close(1011, "broadcast delivery failed");
        } catch {
          /* already gone */
        }
      }
    }
  }

  private ack(
    ws: WebSocket,
    message: { crdt: CrdtType; roomId: string; batchId?: `0x${string}` },
    status: UpdateStatusCode,
    refId?: `0x${string}`
  ): void {
    this.send(ws, {
      type: MessageType.Ack,
      crdt: message.crdt,
      roomId: message.roomId,
      refId: refId ?? message.batchId ?? "0x0000000000000000",
      status
    });
  }

  private newBatchId(): `0x${string}` {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
