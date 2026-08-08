/**
 * DeviceRoom — one Durable Object per device (design §2, §8): a frame relay
 * for interactive RPC + terminal streams + future HTTP tunnel. The host keeps
 * one outbound wss; clients multiplex over `{streamId, kind, bytes}` frames —
 * a generic byte pipe from day one, transport-agnostic by construction (§8.4:
 * a WebRTC fast path could slot in under the same frames).
 *
 * Frame encoding (binary): uleb128 header-length ‖ UTF-8 JSON header ‖ payload.
 * Header: { s: streamId, k: kind, to?: connId, from?: connId }.
 * - client → DO: DO stamps `from = connId` and forwards to the host socket.
 * - host → DO: must carry `to = connId`; DO strips routing keys and delivers.
 *
 * Also holds small "sidecar" JSON slots the host publishes (repos/branches
 * snapshot for instant new-chat pickers §8.1; capability metadata) so pickers
 * render last-known state while the live RPC happens at confirm time.
 */
import { BytesReader, BytesWriter } from "loro-protocol";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import {
  AUTH_CAPABILITIES_HEADER,
  AUTH_GRANT_HEADER,
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  DEVICE_HOST_AUTH_HEADER,
  GRANT_EVENT_HEADER,
  type Env
} from "./env";

const SESSION_READ = "session.read";

export interface TrustedDeviceGrant {
  grantId: string;
  subject: string;
  scope: {
    projectId: string;
    deploymentId: string;
    sessionId: string;
  };
  sandboxId: string;
  targetDeviceId: string;
  capabilities: string[];
  grantedAt: number;
  expiresAt: number;
  revokedAt: number | null;
}

const GRANT_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

export const parseTrustedDeviceGrant = (
  encoded: string | null,
  subject: string,
  projectScope: string,
  now: number
): TrustedDeviceGrant | undefined => {
  if (!encoded || encoded.length > 16 * 1024) return undefined;
  let value: Partial<TrustedDeviceGrant> | null;
  try {
    value = JSON.parse(encoded);
  } catch {
    return undefined;
  }
  if (!value || typeof value !== "object") return undefined;
  if (
    typeof value.grantId !== "string" ||
    !GRANT_ID_RE.test(value.grantId) ||
    value.subject !== subject ||
    !value.scope ||
    typeof value.scope !== "object" ||
    value.scope.projectId !== projectScope ||
    typeof value.scope.deploymentId !== "string" ||
    !GRANT_ID_RE.test(value.scope.deploymentId) ||
    typeof value.scope.sessionId !== "string" ||
    !GRANT_ID_RE.test(value.scope.sessionId) ||
    typeof value.sandboxId !== "string" ||
    !GRANT_ID_RE.test(value.sandboxId) ||
    typeof value.targetDeviceId !== "string" ||
    !GRANT_ID_RE.test(value.targetDeviceId) ||
    !Array.isArray(value.capabilities) ||
    value.capabilities.length === 0 ||
    value.capabilities.length > 32 ||
    value.capabilities.some(
      (capability) => typeof capability !== "string" || capability.length === 0 || capability.length > 128
    ) ||
    !Number.isSafeInteger(value.grantedAt) ||
    !Number.isSafeInteger(value.expiresAt) ||
    (value.grantedAt as number) > now ||
    (value.expiresAt as number) <= now ||
    (value.grantedAt as number) >= (value.expiresAt as number) ||
    value.revokedAt !== null
  ) {
    return undefined;
  }
  return value as TrustedDeviceGrant;
};

export const canonicalGrantEnvelope = (value: TrustedDeviceGrant) => ({
  grant: {
    id: value.grantId,
    principalSubject: value.subject,
    scope: value.scope,
    capabilities: value.capabilities,
    sandboxId: value.sandboxId,
    deviceId: value.targetDeviceId,
    grantedBy: "comet-edge-device-room",
    grantedAt: value.grantedAt,
    expiresAt: value.expiresAt,
    revokedAt: value.revokedAt
  },
  roomId: `s4/${value.scope.projectId}/${value.scope.deploymentId}/${value.scope.sessionId}`,
  targetDeviceId: value.targetDeviceId,
  targetSessionId: value.scope.sessionId
});
const parseCapabilities = (request: Request): string[] =>
  (request.headers.get(AUTH_CAPABILITIES_HEADER) ?? "").split(/\s+/).filter(Boolean);
const hasCapability = (capabilities: readonly string[], capability: string): boolean =>
  capabilities.includes(capability);

export interface DeviceFrameHeader {
  /** Stream id, unique per (connId, logical stream). */
  s: string;
  /** Stream kind: "rpc" | "term" | ... — opaque to the relay. */
  k: string;
  /** Routing: host→client target. */
  to?: string;
  /** Routing: client→host origin (stamped by the relay). */
  from?: string;
}

export const encodeDeviceFrame = (header: DeviceFrameHeader, payload: Uint8Array): Uint8Array => {
  const writer = new BytesWriter();
  writer.pushVarString(JSON.stringify(header));
  writer.pushBytes(payload);
  return writer.finalize();
};

export const decodeDeviceFrame = (
  bytes: Uint8Array
): { header: DeviceFrameHeader; payload: Uint8Array } => {
  const reader = new BytesReader(bytes);
  const header = JSON.parse(reader.readVarString()) as DeviceFrameHeader;
  const payload = reader.readBytes(reader.remaining);
  return { header, payload };
};

interface SocketState {
  userId: string;
  projectScope: string;
  capabilities: string[];
  role: "host" | "client";
  connId: string;
  grant?: TrustedDeviceGrant;
  hostAuthorization?: DeviceHostAuthorization;
  /** Accept time — the liveness floor until the socket's first auto-pong. */
  joinedAt?: number;
  /** A newer socket claimed this logical connection id. */
  superseded?: boolean;
}

const HOST_TAG = "host";
const clientTag = (connId: string) => `client:${connId}`;
export type DeviceHostAuthorization = "local" | "sandbox";

export const authorizedDeviceSocketRole = (
  requestedRole: string | null,
  hasDeviceGrant: boolean,
  hostAuthorization?: DeviceHostAuthorization
): "host" | "client" | undefined => {
  if (requestedRole === "host") {
    const requiredAuthorization = hasDeviceGrant ? "sandbox" : "local";
    return hostAuthorization === requiredAuthorization ? "host" : undefined;
  }
  if (requestedRole === null || requestedRole === "client") {
    return hasDeviceGrant || hostAuthorization !== undefined ? undefined : "client";
  }
  return undefined;
};

export interface DeviceHostAuthorityState {
  readonly hostAuthorization?: DeviceHostAuthorization;
  readonly grant?: Pick<TrustedDeviceGrant, "grantId" | "expiresAt">;
}

export const enforceDeviceHostGrantAuthority = async (
  ws: Pick<WebSocket, "close">,
  state: DeviceHostAuthorityState | null,
  now: number,
  validate: (grantId: string) => Promise<boolean>
): Promise<boolean> => {
  let authorized = state?.hostAuthorization === "local" && state.grant === undefined;
  if (state?.hostAuthorization === "sandbox" && state.grant) {
    const { grantId, expiresAt } = state.grant;
    if (
      GRANT_ID_RE.test(grantId) &&
      Number.isSafeInteger(expiresAt) &&
      expiresAt > now
    ) {
      try {
        authorized = (await validate(grantId)) === true;
      } catch {
        authorized = false;
      }
    }
  }
  if (authorized) return true;
  try {
    ws.close(4403, "device grant invalid");
  } catch {
    /* already gone */
  }
  return false;
};

export const deviceGrantTargetsRoom = (
  credentialTargetDeviceId: string | undefined,
  roomTargetDeviceId: string
): boolean => credentialTargetDeviceId === roomTargetDeviceId;

/** How long a host socket may go without proving liveness before the relay
 * stops routing to it.
 *
 * A host whose network dies silently (laptop lid, NAT/proxy reaping an idle
 * flow) leaves a socket the runtime still reports as connected: no close
 * event ever fires, so `getWebSockets(HOST_TAG)` keeps returning it and the
 * supersede-on-join `close()` never completes either. Picking `[0]` from that
 * list therefore pinned the room to the OLDEST such corpse — every client
 * frame vanished into it while the live host sat later in the list, and
 * clients hung to their own timeouts because a non-empty host list also
 * suppressed the `host_offline` bounce.
 *
 * Hosts ping every 15s (crates/rpc/src/device_room.rs PING_INTERVAL) and the
 * DO's auto-response stamps a timestamp without waking us, so liveness is free
 * to read. The window is sized for the 30s of older builds still in the fleet
 * — 2.5 of their intervals — so upgrading engines is never a prerequisite. */
const HOST_LIVENESS_MS = 75_000;

/** Control frames the relay itself emits (kind " relay"). */
// MUST byte-match packages/rpc device-frames.ts RELAY_KIND — clients compare
// with ===; a mismatch makes host_offline/host_closed invisible to them.
const RELAY_KIND = " relay";

/** Nudge frames (§7 cold-chat command delivery): payload `{chatId}` tells the
 * host "this chat's doc has pending commands — open it and drain". Durable:
 * queued in the DO while the host is offline, replayed on its next join, so a
 * command sent to a chat the host hasn't warm-opened is never stranded. */
export const NUDGE_KIND = "nudge";
export const GRANT_KIND = "grant";
const NUDGE_MAX_PENDING = 256;
const CHAT_ID_RE = /^[A-Za-z0-9_-]{1,64}$/;

export class DeviceRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  private readonly revokedGrants = new Set<string>();

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS pending_nudges (chat_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

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

  /** The host socket to route to: the freshest one that has proven itself
   * alive within [`HOST_LIVENESS_MS`]. `undefined` = no live host, which is
   * what makes clients see `host_offline` instead of hanging on a corpse.
   *
   * `exclude` drops the socket a close is being handled for — the runtime
   * still lists it during `webSocketClose`, and counting it as live would
   * suppress the very `host_closed` that close is supposed to announce. */
  private liveHost(exclude?: WebSocket): WebSocket | undefined {
    return pickLiveHost(
      this.ctx.getWebSockets(HOST_TAG).map((ws) => ({
        ws,
        // Auto-pongs are stamped even while hibernating; `joinedAt` covers the
        // window before a fresh socket's first ping. Sockets attached by an
        // older deploy have neither and read as ancient — correct: they are.
        lastSeenAt: Math.max(
          this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? 0,
          (ws.deserializeAttachment() as SocketState | null)?.joinedAt ?? 0
        )
      })),
      Date.now(),
      exclude
    );
  }

  /** Most recent non-superseded client for a logical connection id.
   * Some runtimes retain a closing WebSocket, so array order is not a
   * sufficient reconnect policy. */
  private liveClient(connId: string): WebSocket | undefined {
    let latest: { ws: WebSocket; joinedAt: number } | undefined;
    for (const ws of this.ctx.getWebSockets(clientTag(connId))) {
      const state = ws.deserializeAttachment() as SocketState | null;
      if (state?.role !== "client" || state.superseded) continue;
      const joinedAt = state.joinedAt ?? 0;
      if (!latest || joinedAt > latest.joinedAt) latest = { ws, joinedAt };
    }
    return latest?.ws;
  }

  async revokeGrant(grantId: string): Promise<void> {
    this.revokedGrants.add(grantId);
    for (const ws of this.ctx.getWebSockets(HOST_TAG)) {
      const state = ws.deserializeAttachment() as SocketState | null;
      if (state?.grant?.grantId !== grantId) continue;
      try {
        ws.close(4403, "device grant revoked");
      } catch {
        /* already gone */
      }
    }
  }

  private async authorizeHost(ws: WebSocket): Promise<boolean> {
    const state = ws.deserializeAttachment() as SocketState | null;
    return enforceDeviceHostGrantAuthority(ws, state, Date.now(), async (grantId) => {
      if (this.revokedGrants.has(grantId)) return false;
      const stub = this.env.AUTH_GRANTS.get(this.env.AUTH_GRANTS.idFromName(grantId));
      const response = await stub.fetch(
        new Request(`https://grant.internal/status?grantId=${encodeURIComponent(grantId)}`, {
          headers: { [GRANT_EVENT_HEADER]: "status" }
        })
      );
      return response.status === 204 && !this.revokedGrants.has(grantId);
    });
  }

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
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const projectScope = request.headers.get(AUTH_PROJECT_HEADER);
    const capabilities = parseCapabilities(request);
    if (!projectScope || !hasCapability(capabilities, SESSION_READ)) {
      return new Response("forbidden", { status: 403 });
    }
    const boundScope = this.getMeta("projectScope");
    if (!boundScope) this.setMeta("projectScope", projectScope);
    else if (boundScope !== projectScope) return new Response("forbidden", { status: 403 });
    const encodedGrant = request.headers.get(AUTH_GRANT_HEADER);
    const grant = parseTrustedDeviceGrant(encodedGrant, userId, projectScope, Date.now());
    if (encodedGrant && !grant) return new Response("invalid grant", { status: 403 });
    const encodedHostAuthorization = request.headers.get(DEVICE_HOST_AUTH_HEADER);
    const hostAuthorization =
      encodedHostAuthorization === "local" || encodedHostAuthorization === "sandbox"
        ? encodedHostAuthorization
        : undefined;
    if (encodedHostAuthorization && !hostAuthorization) {
      return new Response("forbidden", { status: 403 });
    }
    if (grant) {
      if (grant.capabilities.some((capability) => !capabilities.includes(capability))) {
        return new Response("forbidden", { status: 403 });
      }
      const boundDevice = this.getMeta("targetDeviceId");
      if (!boundDevice) this.setMeta("targetDeviceId", grant.targetDeviceId);
      else if (boundDevice !== grant.targetDeviceId) {
        return new Response("forbidden", { status: 403 });
      }
    }
    const owner = this.getMeta("owner");

    if (url.pathname === "/ws") {
      const role = authorizedDeviceSocketRole(
        url.searchParams.get("role"),
        grant !== undefined,
        hostAuthorization
      );
      if (!role) return new Response("forbidden", { status: 403 });
      if (role === "host") {
        if (!hasCapability(capabilities, "session.environment")) {
          return new Response("forbidden", { status: 403 });
        }
        if (hostAuthorization === "local" && this.getMeta("targetDeviceId")) {
          return new Response("forbidden", { status: 403 });
        }
        if (!owner) this.setMeta("owner", projectScope);
        else if (owner !== projectScope) return new Response("forbidden", { status: 403 });
      } else if (!owner || owner !== projectScope) {
        return new Response("forbidden", { status: 403 });
      }
      const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
      const staleClients =
        role === "client" ? this.ctx.getWebSockets(clientTag(connId)) : [];
      const pair = new WebSocketPair();
      if (role === "host") {
        this.ctx.acceptWebSocket(pair[1], [HOST_TAG]);
      } else {
        this.ctx.acceptWebSocket(pair[1], [clientTag(connId)]);
      }
      const state: SocketState = {
        userId,
        projectScope,
        capabilities,
        role,
        connId,
        joinedAt: Date.now(),
        ...(role === "host" ? { hostAuthorization } : {}),
        ...(grant ? { grant } : {})
      };
      pair[1].serializeAttachment(state);
      if (role === "client") {
        for (const stale of staleClients) {
          const staleState = stale.deserializeAttachment() as SocketState | null;
          if (staleState) stale.serializeAttachment({ ...staleState, superseded: true });
          try {
            stale.close(4409, "superseded by new client connection");
          } catch {
            /* already gone */
          }
        }
      }
      if (role === "host") {
        if (!(await this.authorizeHost(pair[1]))) {
          return new Response(null, { status: 101, webSocket: pair[0] });
        }
        // Only an authorized successor may evict the previous host.
        for (const stale of this.ctx.getWebSockets(HOST_TAG)) {
          if (stale === pair[1]) continue;
          try {
            stale.close(4409, "superseded by new host connection");
          } catch {
            /* already gone */
          }
        }
        this.deliverHostStartup(pair[1], grant);
      }
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    // Sidecar slots (host-published JSON, e.g. repos snapshot §8.1).
    const sidecar = url.pathname.match(/^\/sidecar\/([a-z0-9-]{1,64})$/);
    if (sidecar) {
      const name = sidecar[1]!;
      if (!owner || owner !== projectScope) return json({ error: "forbidden" }, owner ? 403 : 404);
      if (request.method === "GET") {
        const value = getJsonBlob<unknown>(this.blobs, `sidecar:${name}`);
        return value === undefined ? json({ error: "not_found" }, 404) : json(value);
      }
      if (request.method === "POST") {
        if (
          !hasCapability(capabilities, "session.files") &&
          !hasCapability(capabilities, "session.environment")
        ) {
          return json({ error: "forbidden" }, 403);
        }
        putJsonBlob(this.blobs, `sidecar:${name}`, await request.json());
        return json({ ok: true });
      }
    }

    if (url.pathname === "/status" && request.method === "GET") {
      if (!owner || owner !== projectScope) return json({ error: "forbidden" }, owner ? 403 : 404);
      // `hostSockets` counts corpses too — the gap between it and
      // `hostConnected` is the only externally visible signal that a device's
      // room is accumulating silently-dead host sockets.
      return json({
        hostConnected: this.liveHost() !== undefined,
        hostSockets: this.ctx.getWebSockets(HOST_TAG).length
      });
    }

    // Durable command nudge (§7). Any authenticated device of the owner may
    // nudge; the payload is only a chat id — the host validates against its
    // own doc before executing anything.
    if (url.pathname === "/nudge" && request.method === "POST") {
      if (
        !owner ||
        owner !== projectScope ||
        (!hasCapability(capabilities, "session.control") &&
          !hasCapability(capabilities, "session.chat"))
      ) {
        return json({ error: "forbidden" }, owner ? 403 : 404);
      }
      const body = (await request.json().catch(() => null)) as { chatId?: string } | null;
      const chatId = body?.chatId;
      if (!chatId || !CHAT_ID_RE.test(chatId)) return json({ error: "bad_chat_id" }, 400);
      const host = this.liveHost();
      if (host && (await this.authorizeHost(host))) {
        this.deliver(host, { s: chatId, k: NUDGE_KIND }, new TextEncoder().encode(JSON.stringify({ chatId })));
        return json({ delivered: true });
      }
      // Host offline: queue durably (dedup by chat — one open covers any
      // number of pending commands), bounded so a runaway sender can't grow
      // the DO forever. Overflow drops the OLDEST: recency wins.
      this.ctx.storage.sql.exec(
        "INSERT INTO pending_nudges (chat_id, queued_at) VALUES (?, ?) ON CONFLICT(chat_id) DO UPDATE SET queued_at = excluded.queued_at",
        chatId,
        Date.now()
      );
      this.ctx.storage.sql.exec(
        "DELETE FROM pending_nudges WHERE chat_id NOT IN (SELECT chat_id FROM pending_nudges ORDER BY queued_at DESC LIMIT ?)",
        NUDGE_MAX_PENDING
      );
      return json({ delivered: false, queued: true });
    }

    return new Response("not found", { status: 404 });
  }

  private replayNudges(host: WebSocket): void {
    const rows = [
      ...this.ctx.storage.sql.exec("SELECT chat_id FROM pending_nudges ORDER BY queued_at ASC")
    ] as Array<{ chat_id: string }>;
    if (rows.length === 0) return;
    for (const row of rows) {
      this.deliver(
        host,
        { s: row.chat_id, k: NUDGE_KIND },
        new TextEncoder().encode(JSON.stringify({ chatId: row.chat_id }))
      );
    }
    this.ctx.storage.sql.exec("DELETE FROM pending_nudges");
  }

  deliverHostStartup(host: WebSocket, grant?: TrustedDeviceGrant): void {
    if (grant) {
      this.deliver(
        host,
        { s: grant.scope.sessionId, k: GRANT_KIND },
        new TextEncoder().encode(JSON.stringify(canonicalGrantEnvelope(grant)))
      );
    }
    this.replayNudges(host);
  }

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    const state = ws.deserializeAttachment() as SocketState | null;
    if (!state) {
      ws.close(1008, "Missing socket authority");
      return;
    }
    if (state.superseded) return;
    if (state.role === "host" && !(await this.authorizeHost(ws))) return;
    if (typeof message === "string") return; // ping/pong auto-response
    let frame: { header: DeviceFrameHeader; payload: Uint8Array };
    try {
      frame = decodeDeviceFrame(new Uint8Array(message));
    } catch {
      ws.close(1002, "Frame error");
      return;
    }
    if (state.role === "client") {
      const actorSubject =
        frame.header.k === "rpc" ? controlActorForRpc(frame.payload) : undefined;
      if (actorSubject !== undefined && actorSubject !== state.userId) {
        this.deliver(
          ws,
          { s: frame.header.s, k: RELAY_KIND },
          encodeRelayError("actor_mismatch")
        );
        return;
      }
      const required = requiredCapabilityForRpc(frame.header, frame.payload);
      if (!hasCapability(state.capabilities, required)) {
        this.deliver(
          ws,
          { s: frame.header.s, k: RELAY_KIND },
          encodeRelayError("capability_denied")
        );
        return;
      }
      const host = this.liveHost();
      if (!host || !(await this.authorizeHost(host))) {
        // Host offline or unauthorized: bounce a relay-level error so the
        // client can surface "device is asleep" instead of hanging.
        this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError("host_offline"));
        return;
      }
      const hostGrant = (host.deserializeAttachment() as SocketState | null)?.grant;
      if (hostGrant && !rpcAllowedForScopedHost(frame.header, frame.payload, hostGrant)) {
        this.deliver(
          ws,
          { s: frame.header.s, k: RELAY_KIND },
          encodeRelayError("session_scope_denied")
        );
        return;
      }
      this.deliver(host, { s: frame.header.s, k: frame.header.k, from: state.connId }, frame.payload);
      return;
    }
    // Host frame: route by `to`.
    const to = frame.header.to;
    if (!to) return;
    const target = this.liveClient(to);
    if (!target) {
      this.deliver(ws, { s: frame.header.s, k: RELAY_KIND, to }, encodeRelayError("client_gone"));
      return;
    }
    this.deliver(target, { s: frame.header.s, k: frame.header.k }, frame.payload);
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    const state = ws.deserializeAttachment() as SocketState | null;
    if (!state) return;
    if (state.superseded) return;
    if (state.role === "client") {
      // Tell the host so it can tear down any per-client streams (ptys etc.).
      const host = this.liveHost();
      if (host && (await this.authorizeHost(host))) {
        this.deliver(host, { s: "", k: RELAY_KIND, from: state.connId }, encodeRelayError("client_closed"));
      }
      return;
    }
    // A host socket went away. Only tear the clients' links down when NO live
    // host is left: a superseded predecessor closing (or a corpse the runtime
    // finally reaps) must not knock clients off the successor that already
    // replaced it.
    if (this.liveHost(ws)) return;
    // Host went away: notify every client.
    for (const client of this.ctx.getWebSockets()) {
      const cs = client.deserializeAttachment() as SocketState | null;
      if (cs?.role !== "client") continue;
      this.deliver(client, { s: "", k: RELAY_KIND }, encodeRelayError("host_closed"));
    }
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    await this.webSocketClose(ws);
  }

  private deliver(ws: WebSocket, header: DeviceFrameHeader, payload: Uint8Array): void {
    try {
      ws.send(encodeDeviceFrame(header, payload));
    } catch {
      /* stale socket */
    }
  }
}

/** Freshest host socket that has proven itself alive inside the liveness
 * window, or `undefined` when every candidate is stale (or there are none).
 * `exclude` skips a socket whose close is being handled — the runtime still
 * lists it there, and counting it as live would suppress the `host_closed`
 * that close exists to announce. Pure so the routing rule is testable without
 * a DO. */
export const pickLiveHost = <T>(
  hosts: ReadonlyArray<{ ws: T; lastSeenAt: number }>,
  now: number,
  exclude?: T
): T | undefined => {
  let best: { ws: T; lastSeenAt: number } | undefined;
  for (const host of hosts) {
    if (host.ws === exclude) continue;
    if (!best || host.lastSeenAt > best.lastSeenAt) best = host;
  }
  return best && now - best.lastSeenAt <= HOST_LIVENESS_MS ? best.ws : undefined;
};

export const controlActorForRpc = (payload: Uint8Array): string | undefined => {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload)) as {
      method?: string;
      params?: { command?: { kind?: string; actorSubject?: string } };
    };
    return value.method === "QueueCommand" && value.params?.command?.kind === "control"
      ? value.params.command.actorSubject
      : undefined;
  } catch {
    return undefined;
  }
};


export const rpcAllowedForScopedHost = (
  header: DeviceFrameHeader,
  payload: Uint8Array,
  grant: TrustedDeviceGrant
): boolean => {
  if (header.k === "term") return grant.capabilities.includes("session.environment");
  if (header.k !== "rpc") return false;
  try {
    const value = JSON.parse(new TextDecoder().decode(payload)) as {
      method?: string;
      params?: Record<string, unknown> & { command?: { sessionId?: string } };
    };
    if (
      value.method === "LocalDevice" &&
      (!value.params || Object.keys(value.params).length === 0)
    ) {
      return true;
    }
    return (
      value.method === "QueueCommand" &&
      value.params?.command?.sessionId === grant.scope.sessionId
    );
  } catch {
    return false;
  }
};

export const requiredCapabilityForRpc = (
  header: DeviceFrameHeader,
  payload: Uint8Array
): string => {
  if (header.k !== "rpc") return header.k === "term" ? "session.environment" : SESSION_READ;
  let value: {
    method?: string;
    params?: { command?: { kind?: string; action?: { action?: string } } };
  };
  try {
    value = JSON.parse(new TextDecoder().decode(payload));
  } catch {
    return "session.control";
  }
  if (value.method === "QueueCommand") {
    const command = value.params?.command;
    if (command?.kind !== "control") return "session.control";
    const action = command.action?.action;
    if (action === "start" || action === "steer") return "session.chat";
    if (action === "environmentLifecycle") return "session.environment";
    if (
      action === "annotationCreate" ||
      action === "annotationEdit" ||
      action === "annotationResolve"
    ) {
      return "session.annotate";
    }
    return "session.control";
  }
  if (value.method?.includes("Attachment") || value.method?.startsWith("Upload")) {
    return "session.files";
  }
  if (value.method?.includes("Invite") || value.method?.includes("Grant")) {
    return "session.invite";
  }
  if (value.method?.includes("Terminal")) return "session.environment";
  if (
    value.method?.startsWith("Watch") ||
    value.method?.startsWith("List") ||
    value.method?.startsWith("Get") ||
    value.method?.startsWith("Search") ||
    value.method === "SyncStatus"
  ) {
    return SESSION_READ;
  }
  return "session.control";
};

const encodeRelayError = (code: string): Uint8Array =>
  new TextEncoder().encode(JSON.stringify({ error: code }));

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
