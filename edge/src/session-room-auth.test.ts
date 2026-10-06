import { Buffer } from "node:buffer";
import { readFileSync } from "node:fs";
import { fileURLToPath, URL as NodeUrl } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CrdtType, JoinErrorCode, MessageType, UpdateStatusCode, decode, encode, type JoinRequest, type ProtocolMessage } from "loro-protocol";
import { LoroDoc } from "loro-crdt";
import type { VersionVector } from "loro-crdt";
import {
  AUTH_CAPABILITIES_HEADER,
  AUTH_GRANT_HEADER,
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  GRANT_EVENT_HEADER,
  ROOM_KIND_HEADER,
  type Env
} from "./env";
import { canonicalSessionId, SessionRoom } from "./session-room";
import worker from "./index";

type SqlRow = Record<string, SqlStorageValue>;

const cursor = (rows: SqlRow[]): SqlStorageCursor<SqlRow> =>
  rows as unknown as SqlStorageCursor<SqlRow>;

class MemorySql {
  readonly meta = new Map<string, string>();
  private readonly blobs = new Map<string, Map<number, ArrayBuffer>>();
  private readonly updates: ArrayBuffer[] = [];

  putBlob(name: string, bytes: Uint8Array): void {
    this.blobs.set(name, new Map([[0, bytes.slice().buffer as ArrayBuffer]]));
  }

  appendUpdate(bytes: Uint8Array): void {
    this.updates.push(bytes.slice().buffer as ArrayBuffer);
  }

  hasBlob(name: string): boolean {
    return this.blobs.has(name);
  }

  updateCount(): number {
    return this.updates.length;
  }

  exec(query: string, ...bindings: unknown[]): SqlStorageCursor<SqlRow> {
    if (bindings.some((value) => value instanceof ArrayBuffer && value.byteLength > 2_000_000)) {
      throw new Error("string or blob too big: SQLITE_TOOBIG");
    }
    const sql = query.replace(/\s+/g, " ").trim().toLowerCase();

    if (sql.startsWith("create table")) return cursor([]);
    if (sql.startsWith("select value from meta")) {
      const value = this.meta.get(String(bindings[0]));
      return cursor(value === undefined ? [] : [{ value }]);
    }
    if (sql.startsWith("insert into meta")) {
      this.meta.set(String(bindings[0]), String(bindings[1]));
      return cursor([]);
    }
    if (sql.startsWith("delete from blobs")) {
      this.blobs.delete(String(bindings[0]));
      return cursor([]);
    }
    if (sql.startsWith("insert into blobs")) {
      const name = String(bindings[0]);
      const chunks = this.blobs.get(name) ?? new Map<number, ArrayBuffer>();
      chunks.set(Number(bindings[1]), bindings[2] as ArrayBuffer);
      this.blobs.set(name, chunks);
      return cursor([]);
    }
    if (sql.startsWith("select sum(length(bytes)) as size from blobs")) {
      const chunks = this.blobs.get(String(bindings[0]));
      return cursor([{
        size: chunks ? [...chunks.values()].reduce((total, bytes) => total + bytes.byteLength, 0) : null
      }]);
    }
    if (sql.startsWith("select bytes from blobs")) {
      const chunks = this.blobs.get(String(bindings[0]));
      if (!chunks) return cursor([]);
      return cursor(
        [...chunks.entries()]
          .sort(([left], [right]) => left - right)
          .map(([, bytes]) => ({ bytes }))
      );
    }
    if (sql.startsWith("insert into updates")) {
      this.updates.push(bindings[0] as ArrayBuffer);
      return cursor([]);
    }
    if (sql === "delete from updates") {
      this.updates.length = 0;
      return cursor([]);
    }
    if (sql.startsWith("select count(*) as n from updates")) {
      return cursor([{ n: this.updates.length }]);
    }
    if (sql.startsWith("select bytes from updates")) {
      return cursor(this.updates.map((bytes) => ({ bytes })));
    }

    throw new Error(`unhandled test SQL: ${query}`);
  }
}

class CapturingSocket {
  readyState: number = WebSocket.OPEN;
  readonly sent: Uint8Array[] = [];
  readonly closed: Array<{ code: number | undefined; reason: string | undefined }> = [];
  private attachment: unknown;

  send(bytes: Uint8Array): void {
    this.sent.push(bytes);
  }

  serializeAttachment(value: unknown): void {
    this.attachment = value;
  }

  deserializeAttachment(): unknown {
    return this.attachment;
  }

  close(code?: number, reason?: string): void {
    this.readyState = WebSocket.CLOSED;
    this.closed.push({ code, reason });
  }
}

const PROJECT_SCOPE = "project-a";
const CAPABILITIES = ["session.read", "session.chat", "session.files", "session.control"];

interface JoinState {
  userId: string;
  projectScope: string;
  capabilities: string[];
  rooms: string[];
  deviceId?: string;
  workspace?: boolean;
  grantId?: string;
  grantExpiresAt?: number;
}

interface SessionRoomInternals {
  eph?: unknown;
  ensureDoc(): Promise<LoroDoc>;
  trimHistoryIfDue(doc: LoroDoc, now: number): Promise<boolean>;
  foldLog(): Promise<void>;
  handleJoin(ws: WebSocket, state: JoinState, message: JoinRequest): Promise<void>;
  applyUpdates(
    ws: WebSocket,
    state: JoinState,
    crdt: CrdtType,
    roomId: string,
    batchId: `0x${string}`,
    updates: Uint8Array[]
  ): Promise<void>;
}

const oversizedPayload = (): Uint8Array => {
  const payload = new Uint8Array(2_100_000);
  let random = 0x12345678;
  for (let i = 0; i < payload.length; i++) {
    random ^= random << 13;
    random ^= random >>> 17;
    random ^= random << 5;
    payload[i] = random & 255;
  }
  return payload;
};

const makeRoom = (
  sql = new MemorySql(),
  sync: () => Promise<void> = async () => {},
  grantStatus: () => Promise<Response> = async () => new Response(null, { status: 200 }),
  putBackup?: (key: string, bytes: Uint8Array) => Promise<void>,
  environment: Env["ENVIRONMENT"] = "local"
): { room: SessionRoom; sql: MemorySql; sockets: WebSocket[] } => {
  const sockets: WebSocket[] = [];
  const storage = {
    sql: sql as unknown as SqlStorage,
    sync,
    transactionSync: <T>(body: () => T): T => body(),
    getAlarm: async () => null,
    setAlarm: async () => {}
  };
  const ctx = {
    storage,
    setWebSocketAutoResponse: () => {},
    acceptWebSocket: (socket: WebSocket) => sockets.push(socket),
    getWebSockets: () => sockets,
    abort: (reason?: string) => {
      throw new Error(reason ?? "aborted");
    }
  } as unknown as DurableObjectState;
  const env = {
    ENVIRONMENT: environment,
    BLOBS: { put: putBackup },
    AUTH_GRANTS: {
      idFromName: (id: string) => id,
      get: () => ({ fetch: grantStatus })
    }
  } as unknown as Env;
  return { room: new SessionRoom(ctx, env), sql, sockets };
};

const authedRequest = (path: string, userId: string, init: RequestInit = {}): Request => {
  const headers = new Headers(init.headers);
  headers.set(AUTH_USER_HEADER, userId);
  headers.set(AUTH_PROJECT_HEADER, PROJECT_SCOPE);
  headers.set(AUTH_CAPABILITIES_HEADER, CAPABILITIES.join(" "));
  return new Request(`https://room.test${path}`, { ...init, headers });
};

const joinRequest = (roomId: string): JoinRequest => ({
  type: MessageType.JoinRequest,
  crdt: CrdtType.Loro,
  roomId,
  auth: new Uint8Array(),
  version: new Uint8Array()
});

const join = async (
  room: SessionRoom,
  userId: string,
  roomId: string,
  version: Uint8Array = new Uint8Array()
): Promise<CapturingSocket> => {
  const socket = new CapturingSocket();
  const state: JoinState = {
    userId,
    projectScope: PROJECT_SCOPE,
    capabilities: CAPABILITIES,
    rooms: []
  };
  socket.serializeAttachment(state);
  await (room as unknown as SessionRoomInternals).handleJoin(
    socket as unknown as WebSocket,
    state,
    { ...joinRequest(roomId), version }
  );
  expect(state.rooms).toContain(CrdtType.Loro);
  expect(socket.sent.some((bytes) => decode(bytes).type === MessageType.JoinResponseOk)).toBe(true);
  return socket;
};

const compactedJoinFixture = () => {
  const source = new LoroDoc();
  const compacted = new LoroDoc();
  try {
    source.setPeerId("1");
    source.getMap("metadata").set("revision", "old");
    source.commit();
    const stale = source.version();
    let staleVersion: Uint8Array;
    try {
      staleVersion = stale.encode();
    } finally {
      stale.free();
    }
    source.getText("history").insert(
      0,
      Array.from({ length: 512 }, (_, i) => `${i}: retained value ${i * i}\n`).join("")
    );
    source.commit();
    const cutoff = source.frontiers();
    const coveredSnapshot = source.export({ mode: "snapshot" });
    source.getMap("metadata").set("revision", "latest");
    source.commit();
    const shallow = source.export({ mode: "shallow-snapshot", frontiers: cutoff });
    compacted.import(shallow);
    const sql = new MemorySql();
    // A log fold persists a regular re-export, then a cold room imports it.
    sql.putBlob("snapshot", compacted.export({ mode: "snapshot" }));
    return {
      room: makeRoom(sql).room,
      staleVersion,
      coveredSnapshot,
      expected: source.toJSON()
    };
  } finally {
    compacted.free();
    source.free();
  }
};

describe("SessionRoom chat authorization", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("WebSocket", { OPEN: 1, CLOSED: 3 });
    vi.stubGlobal(
      "WebSocketRequestResponsePair",
      class {
        constructor(
          readonly request: string,
          readonly response: string
        ) {}
      }
    );
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("lets different authenticated users join within one project scope", async () => {
    const { room, sql } = makeRoom();

    await join(room, "user-a", "shared-chat");
    await join(room, "user-b", "shared-chat");
    expect(sql.meta.get("owner")).toBe(PROJECT_SCOPE);
  });

  it("syncs sandbox project workspaces without widening session, device, or capability authority", async () => {
    const NativeResponse = Response;
    vi.stubGlobal("Response", class extends NativeResponse {
      constructor(body?: BodyInit | null, init?: ResponseInit) {
        super(body, init?.status === 101 ? { ...init, status: 200 } : init);
        if (init?.status === 101) Object.defineProperty(this, "status", { value: 101 });
      }
    });
    vi.stubGlobal("WebSocketPair", class {
      0 = new CapturingSocket();
      1 = new CapturingSocket();
    });
    const grant = {
      userId: "user-a", email: "user-a@example.com", grantId: "1".repeat(32),
      projectId: PROJECT_SCOPE, deploymentId: "deployment-a", sessionId: "assigned-chat",
      sandboxId: "sandbox-a", targetDeviceId: "comet-scaffold-sandbox-a-e1",
      lifecycleEpoch: 1, capabilities: [...CAPABILITIES],
      grantedAt: Date.now() - 1, expiresAt: Date.now() + 600_000, revokedAt: null
    };
    const workspaceRoom = makeRoom();
    const env = {
      SCAFFOLD_PROJECT_SCOPE: PROJECT_SCOPE,
      AUTH_GRANTS: {
        idFromName: (id: string) => id,
        get: () => ({ fetch: async () => Response.json(grant) })
      },
      SESSION_ROOMS: {
        idFromName: (id: string) => id,
        get: () => ({ fetch: (request: Request) => workspaceRoom.room.fetch(request) })
      }
    } as unknown as Env;
    const request = (path: string, method = "GET") => worker.fetch(new Request(`https://edge.test${path}`, {
      method,
      headers: { authorization: `Bearer cs1.${grant.grantId}.${"a".repeat(64)}`, upgrade: "websocket" }
    }), env);
    const roomId = `ws4/${PROJECT_SCOPE}`;
    expect((await request(`/workspace/${PROJECT_SCOPE}/ws`)).status).toBe(101);
    const { room, sockets } = workspaceRoom;
    const publisher = sockets[0] as unknown as CapturingSocket;
    const send = (socket: CapturingSocket, message: ProtocolMessage) =>
      room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
    await send(publisher, joinRequest(roomId));
    expect(publisher.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({
      type: MessageType.JoinResponseOk, permission: "write"
    }));
    const source = new LoroDoc();
    const mirror = new LoroDoc();
    try {
      source.getMap("metadata").set("sandboxStatus", "working");
      const update: ProtocolMessage = {
        type: MessageType.DocUpdate, crdt: CrdtType.Loro, roomId,
        batchId: "0x0000000000000001", updates: [source.export({ mode: "snapshot" })]
      };
      await send(publisher, update);
      expect(publisher.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({
        type: MessageType.Ack, status: UpdateStatusCode.Ok
      }));
      // Another deployment/session shares the workspace, but a read grant cannot publish.
      grant.grantId = "2".repeat(32);
      grant.deploymentId = "deployment-b";
      grant.sessionId = "another-chat";
      grant.capabilities = ["session.read"];
      expect((await request(`/workspace/${PROJECT_SCOPE}/ws`)).status).toBe(101);
      const reader = sockets[1] as unknown as CapturingSocket;
      await send(reader, joinRequest(roomId));
      for (const bytes of reader.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) mirror.importBatch(message.updates);
      }
      expect(mirror.getMap("metadata").get("sandboxStatus")).toBe("working");
      reader.sent.length = 0;
      await send(reader, update);
      expect(reader.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({
        type: MessageType.Ack, status: UpdateStatusCode.PermissionDenied
      }));
      await room.fetch(new Request("https://room.test/grant-revoked", {
        method: "POST", headers: { [GRANT_EVENT_HEADER]: "revoke" },
        body: JSON.stringify({ grantId: "1".repeat(32) })
      }));
      expect(publisher.closed).toContainEqual({ code: 4403, reason: "device grant revoked" });
      expect(reader.closed).toEqual([]);
    } finally {
      source.free();
      mirror.free();
    }
    grant.capabilities = [...CAPABILITIES];
    for (const [path, method] of [
      ["/workspace/other-project/ws", "GET"],
      ["/session/00000000-0000-4000-8000-000000000099/ws", "GET"],
      ["/device/unrelated-device/ws?role=host", "GET"],
      ["/notifications/device", "PUT"],
      [`/workspace/${PROJECT_SCOPE}/reset-log`, "POST"],
      [`/attachments/${"a".repeat(64)}`, "GET"],
      ["/auth/device-grants", "POST"]
    ] as const) expect((await request(path, method)).status, path).toBe(403);
    grant.capabilities = ["session.chat"];
    expect((await request(`/workspace/${PROJECT_SCOPE}/ws`)).status).toBe(403);

    const encodedGrant = JSON.stringify({
      ...grant, subject: grant.userId,
      scope: { projectId: PROJECT_SCOPE, deploymentId: grant.deploymentId,
        sessionId: grant.sessionId, lifecycleEpoch: grant.lifecycleEpoch }
    });
    for (const [chatId, workspace] of [["unrelated-chat", false], ["ws4/other-project", true]] as const) {
      expect((await makeRoom().room.fetch(authedRequest(`/ws?chatId=${chatId}`, grant.userId, {
        headers: { [AUTH_GRANT_HEADER]: encodedGrant, ...(workspace ? { [ROOM_KIND_HEADER]: "workspace" } : {}) }
      }))).status).toBe(403);
    }
    vi.advanceTimersByTime(600_000);
    const reader = sockets[1] as unknown as CapturingSocket;
    await send(reader, joinRequest(roomId));
    expect(reader.closed).toContainEqual({ code: 4403, reason: "device grant invalid" });
  });

  it("preserves durable state when four readers cold-start the same room", async () => {
    const source = new LoroDoc();
    const map = source.getMap("metadata");
    try {
      map.set("retained", "must survive simultaneous joins");
      const sql = new MemorySql();
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "concurrent-cold-chat");
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql);
      const responses = await Promise.all(Array.from({ length: 4 }, () =>
        room.fetch(authedRequest("/snapshot", "user-a"))));
      for (const response of responses) {
        expect(response.status).toBe(200);
        const mirror = new LoroDoc();
        try {
          mirror.import(new Uint8Array(await response.arrayBuffer()));
          expect(mirror.toJSON()).toEqual(source.toJSON());
        } finally { mirror.free(); }
      }
    } finally { map.free(); source.free(); }
  });

  it("accepts two independent warm deltas without rereading retained history", async () => {
    const first = new LoroDoc();
    const second = new LoroDoc();
    const mirror = new LoroDoc();
    let base: VersionVector | undefined;
    try {
      first.getMap("metadata").set("payload", oversizedPayload());
      first.commit();
      first.getMap("metadata").set("payload", "current");
      first.commit();
      const baseline = first.export({ mode: "snapshot" });
      second.import(baseline);
      base = first.oplogVersion();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.putBlob("snapshot", baseline);
      const { room } = makeRoom(sql);
      const internals = room as unknown as SessionRoomInternals;
      await internals.ensureDoc();
      const exec = sql.exec.bind(sql);
      const noHistoricalReads = vi.spyOn(sql, "exec").mockImplementation((query, ...bindings) => {
        if (/^SELECT bytes FROM (blobs|updates)/i.test(query)) throw new Error("historical reads unavailable on warm path");
        return exec(query, ...bindings);
      });
      try {
        first.getMap("metadata").set("first", true);
        first.commit();
        second.getMap("metadata").set("second", true);
        second.commit();
        for (const peer of [first, second]) {
          expect((await room.fetch(authedRequest("/append", "user-a", {
            method: "POST", body: peer.export({ mode: "update", from: base })
          }))).status).toBe(200);
        }
        expect((await internals.ensureDoc()).toJSON()).toEqual({ metadata: { payload: "current", first: true, second: true } });
      } finally { noHistoricalReads.mockRestore(); }
      await room.fetch(authedRequest("/stats", "user-a"));
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await cold.arrayBuffer()));
      expect(mirror.toJSON()).toEqual({ metadata: { payload: "current", first: true, second: true } });
    } finally { base?.free(); mirror.free(); second.free(); first.free(); }
  });

  it("folds the current replica when a snapshot replaces it during a no-op trim", async () => {
    const source = new LoroDoc();
    const mirror = new LoroDoc();
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    try {
      source.getMap("metadata").set("before", true);
      source.commit();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql);
      const internals = room as unknown as SessionRoomInternals;
      await internals.ensureDoc();
      const trimming = vi.spyOn(internals, "trimHistoryIfDue").mockImplementationOnce(async () => {
        entered.resolve();
        await resume.promise;
        return false;
      });
      const folding = internals.foldLog();
      try {
        await entered.promise;
        source.getMap("metadata").set("duringFold", true);
        source.commit();
        expect((await room.fetch(authedRequest("/append", "user-a", {
          method: "POST", body: source.export({ mode: "snapshot" })
        }))).status).toBe(200);
      } finally { resume.resolve(); await folding; trimming.mockRestore(); }
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await cold.arrayBuffer()));
      expect(mirror.toJSON()).toEqual(source.toJSON());
    } finally { resume.resolve(); mirror.free(); source.free(); }
  });

  it("keeps backup bytes and version aligned when a snapshot replaces the replica during R2 persistence", async () => {
    const source = new LoroDoc();
    const backed = new LoroDoc();
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let uploaded: Uint8Array | undefined;
    let pause = true;
    try {
      source.getMap("metadata").set("before", true);
      source.commit();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "ws4/project-a");
      sql.meta.set("backupDirty", "1");
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql, undefined, undefined, async (_key, bytes) => {
        uploaded = bytes;
        if (pause) {
          pause = false;
          entered.resolve();
          await resume.promise;
        }
      });
      const backingUp = room.alarm();
      try {
        await entered.promise;
        source.getMap("metadata").set("duringBackup", true);
        source.commit();
        expect((await room.fetch(authedRequest("/append", "user-a", {
          method: "POST", body: source.export({ mode: "snapshot" })
        }))).status).toBe(200);
      } finally { resume.resolve(); await backingUp; }
      backed.import(uploaded!);
      expect(backed.toJSON()).toEqual({ metadata: { before: true } });
      let version = backed.oplogVersion();
      try { expect(sql.meta.get("backupVV")).toBe(Buffer.from(version.encode()).toString("base64")); }
      finally { version.free(); }
      expect(sql.meta.get("backupDirty")).toBe("1");
      await room.alarm();
      backed.import(uploaded!);
      expect(backed.toJSON()).toEqual(source.toJSON());
      version = backed.oplogVersion();
      try { expect(sql.meta.get("backupVV")).toBe(Buffer.from(version.encode()).toString("base64")); }
      finally { version.free(); }
      expect(sql.meta.get("backupDirty")).toBe("0");
    } finally { resume.resolve(); backed.free(); source.free(); }
  });

  it("merges compatible concurrent snapshots across cold restart without client resync", async () => {
    const first = new LoroDoc();
    const second = new LoroDoc();
    const firstMap = first.getMap("metadata");
    const secondMap = second.getMap("metadata");
    const mirror = new LoroDoc();
    try {
      firstMap.set("firstPeer", "retained baseline");
      secondMap.set("secondPeer", "incoming branch");
      const sql = new MemorySql();
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "concurrent-snapshot-chat");
      sql.putBlob("snapshot", first.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql);
      const incoming = second.export({ mode: "snapshot" });
      expect((await room.fetch(authedRequest("/append", "user-a", {
        method: "POST", body: incoming
      }))).status).toBe(200);
      first.import(incoming);
      const restarted = makeRoom(sql).room;
      const response = await restarted.fetch(authedRequest("/snapshot", "user-a"));
      expect(response.status).toBe(200);
      mirror.import(new Uint8Array(await response.arrayBuffer()));
      expect(mirror.toJSON()).toEqual(first.toJSON());
    } finally {
      firstMap.free(); secondMap.free(); mirror.free(); second.free(); first.free();
    }
  });

  it.each([1, 2])("preserves both branches at a shallow boundary after %i server writes", async (serverWrites) => {
    const source = new LoroDoc();
    const offline = new LoroDoc();
    const mirror = new LoroDoc();
    try {
      source.getMap("metadata").set("base", true);
      source.commit();
      offline.import(source.export({ mode: "snapshot" }));
      offline.getMap("metadata").set("offline", "retained locally");
      offline.commit();
      source.getMap("metadata").set("server", "retained remotely");
      source.commit();
      // The frontier's own operation remains exportable. A second server
      // operation moves shallowSinceVV beyond the offline writer's base.
      if (serverWrites === 2) {
        source.getMap("metadata").set("afterGap", true);
        source.commit();
      }
      const sql = new MemorySql();
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("roomKind", "workspace");
      sql.putBlob("snapshot", source.export({ mode: "shallow-snapshot", frontiers: source.frontiers() }));
      const { room } = makeRoom(sql);
      expect((await room.fetch(authedRequest("/append", "user-a", {
        method: "POST", body: offline.export({ mode: "snapshot" })
      }))).status).toBe(serverWrites === 1 ? 200 : 400);
      if (serverWrites === 1) source.import(offline.export({ mode: "snapshot" }));
      expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(source.toJSON());
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await cold.arrayBuffer()));
      expect(mirror.toJSON()).toEqual(source.toJSON());
      expect(offline.toJSON()).toEqual({ metadata: { base: true, offline: "retained locally" } });
    } finally { mirror.free(); offline.free(); source.free(); }
  });

  it.each(["pending", "malformed"] as const)("rejects a %s batch without contaminating live or persisted state", async (failure) => {
    const source = new LoroDoc();
    const missing = new LoroDoc();
    const mirror = new LoroDoc();
    let before: VersionVector | undefined;
    let dependencyVersion: VersionVector | undefined;
    try {
      source.getMap("metadata").set("baseline", true);
      source.commit();
      const sql = new MemorySql();
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql);
      const socket = await join(room, "user-a", "atomic-chat");
      const internals = room as unknown as SessionRoomInternals;
      before = source.oplogVersion();
      source.getMap("metadata").set("unaccepted", true);
      source.commit();
      missing.getMap("metadata").set("dependency", true);
      missing.commit();
      const dependency = missing.export({ mode: "snapshot" });
      dependencyVersion = missing.oplogVersion();
      missing.getMap("metadata").set("unresolved", true);
      missing.commit();
      socket.sent.length = 0;
      await internals.applyUpdates(socket as unknown as WebSocket, socket.deserializeAttachment() as JoinState,
        CrdtType.Loro, "atomic-chat", "0x0000000000000001", [
          source.export({ mode: "update", from: before }),
          failure === "pending" ? missing.export({ mode: "update", from: dependencyVersion }) : new Uint8Array([1, 2, 3])
        ]);
      expect(socket.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({
        type: MessageType.Ack, status: UpdateStatusCode.InvalidUpdate
      })]);
      expect((await internals.ensureDoc()).toJSON()).toEqual({ metadata: { baseline: true } });
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await cold.arrayBuffer()));
      expect(mirror.toJSON()).toEqual({ metadata: { baseline: true } });
      // Supplying the missing dependency later must not resurrect rejected ops.
      expect((await room.fetch(authedRequest("/append", "user-a", { method: "POST", body: dependency }))).status).toBe(200);
      expect((await internals.ensureDoc()).toJSON()).toEqual({ metadata: { baseline: true, dependency: true } });
    } finally {
      dependencyVersion?.free(); before?.free(); mirror.free(); missing.free(); source.free();
    }
  });

  it("preserves offline session discovery after a client uploads a newer shallow snapshot", async () => {
    const source = new LoroDoc();
    const offline = new LoroDoc();
    const catchup = new LoroDoc();
    const reader = new LoroDoc();
    let offlineBase: VersionVector | undefined;
    try {
      source.setPeerId("10");
      source.getMap("devices").set("desktop", { id: "desktop", platform: "macos" });
      source.getMap("chats").set("base", { id: "base", deviceId: "desktop" });
      source.commit();
      offline.import(source.export({ mode: "snapshot" }));
      offline.setPeerId("20");
      offlineBase = offline.oplogVersion();
      offline.getMap("chats").set("offline", { id: "offline", deviceId: "desktop" });
      offline.getMap("sessionRefs").set("6:user-a:offline", { chatId: "offline", userId: "user-a", addedAt: 1 });
      offline.commit();
      source.getMap("chats").set("server", { id: "server", deviceId: "desktop" });
      source.commit();
      catchup.import(source.export({ mode: "snapshot" }));
      catchup.setPeerId("30");
      catchup.getMap("sessionRefs").set("6:user-a:server", { chatId: "server", userId: "user-a", addedAt: 2 });
      catchup.commit();

      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("chatId", "ws4/project-a");
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const headers = { [ROOM_KIND_HEADER]: "workspace" };
      expect((await makeRoom(sql).room.fetch(authedRequest("/append", "user-a", {
        method: "POST", headers,
        body: catchup.export({ mode: "shallow-snapshot", frontiers: catchup.frontiers() })
      }))).status).toBe(200);
      const restarted = makeRoom(sql).room;
      expect((await restarted.fetch(authedRequest("/append", "user-a", {
        method: "POST", headers, body: offline.export({ mode: "update", from: offlineBase })
      }))).status).toBe(200);
      source.import(catchup.export({ mode: "snapshot" }));
      source.import(offline.export({ mode: "update", from: offlineBase }));
      await restarted.fetch(authedRequest("/stats", "user-a", { headers }));
      const socket = await join(makeRoom(sql).room, "user-a", "ws4/project-a");
      for (const bytes of socket.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) {
          for (const update of message.updates) expect(reader.import(update).pending?.size ?? 0).toBe(0);
        }
      }
      expect(reader.toJSON()).toEqual(source.toJSON());
      expect(reader.getMap("sessionRefs").get("6:user-a:offline")).toEqual({ chatId: "offline", userId: "user-a", addedAt: 1 });
    } finally {
      offlineBase?.free(); reader.free(); catchup.free(); offline.free(); source.free();
    }
  });

  it.each([
    ["workspace", "age"], ["workspace", "size"],
    ["legacy", "age"], ["legacy", "size"]
  ] as const)("preserves offline workspace edits through %s %s folds and fresh-reader backfill", async (kind, trigger) => {
    const source = new LoroDoc();
    const writer = new LoroDoc();
    const reader = new LoroDoc();
    let writerBase: VersionVector | undefined;
    try {
      source.getMap("metadata").set("base", true);
      source.commit();
      const baseline = source.export({ mode: "snapshot" });
      writer.import(baseline);
      writerBase = writer.oplogVersion();
      writer.getMap("metadata").set("offline", "retained locally");
      writer.commit();

      const sql = new MemorySql();
      sql.putBlob("snapshot", baseline);
      if (kind === "workspace") sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "ws4/project-a");
      const internals = makeRoom(sql).room as unknown as SessionRoomInternals;
      const live = await internals.ensureDoc();
      source.getMap("metadata").set("server", trigger === "size" ? oversizedPayload() : "initial");
      source.commit();
      source.getMap("metadata").set("server", "retained remotely");
      source.commit();
      source.getMap("metadata").set("afterGap", true);
      source.commit();
      const serverDelta = source.export({ mode: "update", from: writerBase });
      sql.appendUpdate(serverDelta);
      sql.meta.set("updateBytes", String(serverDelta.byteLength));
      live.import(serverDelta);
      if (trigger === "age") {
        sql.meta.set("checkpoints", JSON.stringify([{
          at: Date.now() - 365 * 24 * 60 * 60 * 1000,
          frontiers: live.frontiers()
        }]));
      }
      await internals.foldLog();

      const restarted = makeRoom(sql).room;
      const headers = { [ROOM_KIND_HEADER]: "workspace" };
      expect((await restarted.fetch(authedRequest("/append", "user-a", {
        method: "POST", headers, body: writer.export({ mode: "update", from: writerBase })
      }))).status).toBe(200);
      // Persist the offline branch before a second cold reader backfills it.
      expect((await restarted.fetch(authedRequest("/snapshot", "user-a", { headers }))).status).toBe(200);
      const socket = await join(makeRoom(sql).room, "user-b", "ws4/project-a");
      const fragments = new Map<string, { parts: Uint8Array[]; remaining: number; size: number }>();
      for (const bytes of socket.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) {
          for (const update of message.updates) expect(reader.import(update).pending?.size ?? 0).toBe(0);
        } else if (message.type === MessageType.DocUpdateFragmentHeader) {
          fragments.set(message.batchId, {
            parts: new Array(message.fragmentCount), remaining: message.fragmentCount, size: message.totalSizeBytes
          });
        } else if (message.type === MessageType.DocUpdateFragment) {
          const batch = fragments.get(message.batchId)!;
          batch.parts[message.index] = message.fragment;
          if (--batch.remaining !== 0) continue;
          const update = new Uint8Array(batch.size);
          let offset = 0;
          for (const part of batch.parts) {
            update.set(part, offset);
            offset += part.length;
          }
          expect(reader.import(update).pending?.size ?? 0).toBe(0);
          fragments.delete(message.batchId);
        }
      }
      expect(reader.toJSON()).toEqual({ metadata: {
        base: true, offline: "retained locally", server: "retained remotely", afterGap: true
      } });
    } finally {
      writerBase?.free(); reader.free(); writer.free(); source.free();
    }
  });

  it.each(["staging", "production"] as const)("rejects invalid recovery seeds without replacing %s history", async (environment) => {
    const source = new LoroDoc();
    const recovered = new LoroDoc();
    try {
      source.getMap("metadata").set("retained", "accepted state");
      source.commit();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql, undefined, undefined, undefined, environment);
      const invalid = new Uint8Array([1, 2, 3]);
      const headers = { [ROOM_KIND_HEADER]: "workspace" };
      const response = await room.fetch(authedRequest("/reset-log", "user-a", {
        method: "POST", headers, body: invalid
      }));
      expect(response.status).toBe(400);
      const diagnostic = await response.json() as { sha256?: string; seedBytes?: number; validationStage?: string };
      if (environment === "staging") {
        const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", invalid));
        expect(diagnostic.sha256).toBe(Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(""));
        expect(diagnostic.seedBytes).toBe(invalid.byteLength);
      } else {
        expect(diagnostic.sha256).toBeUndefined();
        expect(diagnostic.validationStage).toBeUndefined();
      }
      const snapshot = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a", { headers }));
      recovered.import(new Uint8Array(await snapshot.arrayBuffer()));
      expect(recovered.toJSON()).toEqual({ metadata: { retained: "accepted state" } });
    } finally {
      recovered.free(); source.free();
    }
  });

  it("atomically reseeds a workspace reset from one bounded complete snapshot", async () => {
    const previous = new LoroDoc();
    const replacement = new LoroDoc();
    const restored = new LoroDoc();
    try {
      previous.getMap("metadata").set("previous", true);
      replacement.getMap("metadata").set("canonical", true);
      const sql = new MemorySql();
      sql.putBlob("snapshot", previous.export({ mode: "snapshot" }));
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "ws4/project-a");
      const { room } = makeRoom(sql);

      const oversized = await room.fetch(authedRequest("/reset-log", "user-a", {
        method: "POST", headers: { [ROOM_KIND_HEADER]: "workspace" },
        body: new Uint8Array(8 * 1024 * 1024 + 1)
      }));
      expect(oversized.status).toBe(413);
      expect(sql.hasBlob("snapshot")).toBe(true);

      const seed = replacement.export({
        mode: "shallow-snapshot", frontiers: replacement.frontiers()
      });
      const response = await room.fetch(authedRequest("/reset-log", "user-a", {
        method: "POST", headers: { [ROOM_KIND_HEADER]: "workspace" }, body: seed
      }));
      expect(response.status).toBe(200);
      await expect(response.json()).resolves.toMatchObject({ ok: true, seedBytes: seed.byteLength });

      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      restored.import(new Uint8Array(await cold.arrayBuffer()));
      expect(restored.toJSON()).toEqual({ metadata: { canonical: true } });
      expect(sql.updateCount()).toBe(0);
      expect(sql.meta.get("postReset")).toBe("0");
    } finally {
      restored.free(); replacement.free(); previous.free();
    }
  });

  it("bootstraps out-of-order pending deltas without losing them across cold replay", async () => {
    const source = new LoroDoc();
    const map = source.getMap("metadata");
    let before: VersionVector | undefined;
    const mirror = new LoroDoc();
    try {
      map.set("base", "preserved");
      source.commit();
      const snapshot = source.export({ mode: "snapshot" });
      before = source.version();
      map.set("late", "retained");
      source.commit();
      const delta = source.export({ mode: "update", from: before });
      const intermediate = source.version();
      let later: Uint8Array;
      try {
        map.set("last", "also retained");
        source.commit();
        later = source.export({ mode: "update", from: intermediate });
      } finally { intermediate.free(); }
      const sql = new MemorySql();
      sql.appendUpdate(later);
      sql.appendUpdate(delta);
      const { room } = makeRoom(sql);
      const socket = new CapturingSocket();
      const state: JoinState = { userId: "user-a", projectScope: PROJECT_SCOPE, capabilities: CAPABILITIES, rooms: [] };
      socket.serializeAttachment(state);
      const internals = room as unknown as SessionRoomInternals;
      await internals.handleJoin(socket as unknown as WebSocket, state, joinRequest("bootstrap-chat"));
      expect(socket.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({
        type: MessageType.JoinError, code: JoinErrorCode.AppError, message: "incomplete_history"
      })]);
      expect(state.rooms).toEqual([]);
      socket.sent.length = 0;
      await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode({
        type: MessageType.DocUpdate, crdt: CrdtType.Loro, roomId: "bootstrap-chat",
        batchId: "0x0000000000000001", updates: [delta]
      })).buffer);
      expect(socket.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({
        type: MessageType.Ack, status: UpdateStatusCode.InvalidUpdate
      })]);
      socket.sent.length = 0;
      await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode({
        type: MessageType.DocUpdate, crdt: CrdtType.Loro, roomId: "bootstrap-chat",
        batchId: "0x0000000000000002", updates: [snapshot]
      })).buffer);
      expect(socket.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({
        type: MessageType.Ack, status: UpdateStatusCode.Ok
      })]);
      expect(state.rooms).toEqual([]);
      socket.sent.length = 0;
      await internals.handleJoin(socket as unknown as WebSocket, state, joinRequest("bootstrap-chat"));
      expect(socket.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.JoinResponseOk }));
      for (const bytes of socket.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) {
          for (const update of message.updates) expect(mirror.import(update).pending?.size ?? 0).toBe(0);
        }
      }
      expect(mirror.toJSON()).toEqual(source.toJSON());
      expect((await internals.ensureDoc()).toJSON()).toEqual(source.toJSON());
      const restarted = makeRoom(sql).room;
      const replay = await restarted.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await replay.arrayBuffer()));
      expect(mirror.toJSON()).toEqual(source.toJSON());
    } finally {
      before?.free();
      map.free();
      mirror.free();
      source.free();
    }
  });

  it("rejects an earlier unresolved delta even when the last replay row is covered", async () => {
    const source = new LoroDoc();
    const map = source.getMap("metadata");
    const unrelated = new LoroDoc();
    const unrelatedMap = unrelated.getMap("metadata");
    let before: VersionVector | undefined;
    const mirror = new LoroDoc();
    try {
      map.set("base", "preserved");
      source.commit();
      before = source.version();
      map.set("late", "retained");
      source.commit();
      const delta = source.export({ mode: "update", from: before });
      const sql = new MemorySql();
      sql.appendUpdate(delta);
      unrelatedMap.set("old", true);
      sql.appendUpdate(unrelated.export({ mode: "snapshot" }));
      const { room } = makeRoom(sql);
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("chatId", "bootstrap-chat");
      unrelatedMap.set("unrelated", true);
      const rejected = await room.fetch(authedRequest("/append", "user-a", {
        method: "POST", body: unrelated.export({ mode: "snapshot" })
      }));
      expect(rejected.status).toBe(400);
      source.import(unrelated.export({ mode: "snapshot" }));
      const accepted = await room.fetch(authedRequest("/append", "user-a", { method: "POST", body: source.export({ mode: "snapshot" }) }));
      expect(accepted.status).toBe(200);
      const replay = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await replay.arrayBuffer()));
      expect(mirror.toJSON()).toEqual(source.toJSON());
    } finally {
      before?.free();
      map.free();
      unrelatedMap.free();
      mirror.free();
      unrelated.free();
      source.free();
    }
  });

  it(
    "fully backfills a stale client after compacted room rematerialization",
    async () => {
      const { room, staleVersion, expected } = compactedJoinFixture();
      const socket = await join(room, "user-a", "retained-chat", staleVersion);
      const backfill = socket.sent
        .map((bytes) => decode(bytes))
        .find((message) => message.type === MessageType.DocUpdate);
      expect(backfill?.type).toBe(MessageType.DocUpdate);
      // Native recovery installs a validated replacement replica: importing a
      // truncated snapshot into an old nonempty replica can itself stay pending.
      const recovered = new LoroDoc();
      try {
        if (backfill?.type === MessageType.DocUpdate) {
          for (const update of backfill.updates) {
            expect(recovered.import(update).pending?.size ?? 0).toBe(0);
          }
        }
        expect(recovered.toJSON()).toEqual(expected);
        const state = recovered.version();
        const oplog = recovered.oplogVersion();
        try {
          expect(state.compare(oplog)).toBe(0);
        } finally {
          oplog.free();
          state.free();
        }
      } finally {
        recovered.free();
      }
    }
  );

  it(
    "incrementally catches up a covered client after compacted room rematerialization",
    async () => {
      const { room, coveredSnapshot, expected } = compactedJoinFixture();
      const mirror = new LoroDoc();
      try {
        mirror.import(coveredSnapshot);
        const version = mirror.version();
        let socket: CapturingSocket;
        try {
          socket = await join(room, "user-a", "retained-chat", version.encode());
        } finally {
          version.free();
        }
        const backfill = socket.sent
          .map((bytes) => decode(bytes))
          .find((message) => message.type === MessageType.DocUpdate);
        expect(backfill?.type).toBe(MessageType.DocUpdate);
        if (backfill?.type === MessageType.DocUpdate) {
          for (const update of backfill.updates) {
            expect(mirror.import(update).pending?.size ?? 0).toBe(0);
          }
        }
        expect(mirror.toJSON()).toEqual(expected);
      } finally {
        mirror.free();
      }
    }
  );

  it("replays bounded workspace presence without allocating another WASM store", async () => {
    const { room, sockets } = makeRoom();
    const internals = room as unknown as SessionRoomInternals;
    const roomId = "ws4/project-a";
    const source = new CapturingSocket();
    const target = new CapturingSocket();
    const state = (deviceId: string): JoinState => ({
      userId: "user-a",
      projectScope: PROJECT_SCOPE,
      capabilities: CAPABILITIES,
      rooms: [CrdtType.Loro],
      workspace: true,
      deviceId
    });
    const sourceState = state("device-a");
    const targetState = state("device-b");
    source.serializeAttachment(sourceState);
    target.serializeAttachment(targetState);
    sockets.push(source as unknown as WebSocket, target as unknown as WebSocket);
    const join = { ...joinRequest(roomId), crdt: CrdtType.LoroEphemeralStore };

    await internals.handleJoin(source as unknown as WebSocket, sourceState, join);
    expect(internals.eph).toBeUndefined();

    const heartbeat = new Uint8Array([1, 2, 3]);
    await internals.applyUpdates(
      source as unknown as WebSocket,
      sourceState,
      CrdtType.LoroEphemeralStore,
      roomId,
      "0x0000000000000001",
      [heartbeat]
    );

    expect(internals.eph).toBeUndefined();
    const sourceMessages = source.sent.map((bytes) => decode(bytes));
    expect(
      sourceMessages.some(
        (message) => message.type === MessageType.Ack && message.status === 0
      )
    ).toBe(true);

    await internals.handleJoin(target as unknown as WebSocket, targetState, join);
    const relayed = target.sent
      .map((bytes) => decode(bytes))
      .find((message) => message.type === MessageType.DocUpdate);
    expect(relayed?.type).toBe(MessageType.DocUpdate);
    if (relayed?.type === MessageType.DocUpdate) {
      expect(relayed.crdt).toBe(CrdtType.LoroEphemeralStore);
      expect(relayed.updates).toEqual([heartbeat]);
    }

    vi.advanceTimersByTime(30_001);
    const expired = new CapturingSocket();
    const expiredState = state("device-c");
    expired.serializeAttachment(expiredState);
    sockets.push(expired as unknown as WebSocket);
    await internals.handleJoin(expired as unknown as WebSocket, expiredState, join);
    expect(
      expired.sent.some((bytes) => decode(bytes).type === MessageType.DocUpdate)
    ).toBe(false);
  });

  it("assembles out-of-order fragments once despite retransmitted indices", async () => {
    const { room } = makeRoom();
    const socket = await join(room, "user-a", "fragment-chat");
    socket.sent.length = 0;
    const source = new LoroDoc();
    try {
      source.getText("text").insert(0, "fragmented transcript");
      const update = source.export({ mode: "snapshot" });
      const middle = Math.floor(update.length / 2);
      const envelope = {
        crdt: CrdtType.Loro,
        roomId: "fragment-chat",
        batchId: "0x0000000000000001" as const
      };
      const send = async (message: ProtocolMessage) => {
        await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
      };
      await send({
        type: MessageType.DocUpdateFragmentHeader,
        ...envelope,
        fragmentCount: 2,
        totalSizeBytes: update.length
      });
      const second: ProtocolMessage = {
        type: MessageType.DocUpdateFragment,
        ...envelope,
        index: 1,
        fragment: update.subarray(middle)
      };
      await send(second);
      await send(second);
      expect(socket.sent).toEqual([]);
      await send({
        type: MessageType.DocUpdateFragment,
        ...envelope,
        index: 0,
        fragment: update.subarray(0, middle)
      });
      expect(socket.sent.map((bytes) => decode(bytes))).toEqual([{
        type: MessageType.Ack,
        crdt: CrdtType.Loro,
        roomId: "fragment-chat",
        refId: envelope.batchId,
        status: UpdateStatusCode.Ok
      }]);
      const received = await (room as unknown as SessionRoomInternals).ensureDoc();
      expect(received.getText("text").toString()).toBe("fragmented transcript");
    } finally {
      source.free();
    }
  });

  it("rejects a grant revoked while a cold document is materializing", async () => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    let authorized = true;
    const { room } = makeRoom(new MemorySql(), async () => {
      if (!pause) return;
      pause = false;
      entered.resolve();
      await resume.promise;
    }, async () => new Response(null, { status: authorized ? 200 : 403 }));
    const socket = await join(room, "user-a", "authority-chat");
    const state = socket.deserializeAttachment() as JoinState;
    state.grantId = "grant-1";
    state.grantExpiresAt = Date.now() + 600_000;
    socket.sent.length = 0;
    vi.advanceTimersByTime(61_000);
    const source = new LoroDoc();
    source.getMap("metadata").set("forbidden", true);
    pause = true;
    const applying = room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode({
      type: MessageType.DocUpdate,
      crdt: CrdtType.Loro,
      roomId: "authority-chat",
      batchId: "0x0000000000000001",
      updates: [source.export({ mode: "update" })]
    })).buffer);
    try {
      await Promise.race([entered.promise, applying]);
      expect(pause).toBe(false);
      // The authority revokes before its notification reaches the room.
      authorized = false;
      resume.resolve();
      await applying;
      expect(socket.closed).toContainEqual({ code: 4403, reason: "device grant invalid" });
      expect(socket.sent).toEqual([]);
      const live = await (room as unknown as SessionRoomInternals).ensureDoc();
      expect(live.getMap("metadata").get("forbidden")).toBeUndefined();
      const stats = await room.fetch(authedRequest("/stats", "user-a"));
      expect(stats.status).toBe(200);
      const snapshot = await room.fetch(authedRequest("/snapshot", "user-a"));
      const mirror = new LoroDoc();
      try {
        mirror.import(new Uint8Array(await snapshot.arrayBuffer()));
        expect(mirror.getMap("metadata").get("forbidden")).toBeUndefined();
      } finally {
        mirror.free();
      }
    } finally {
      resume.resolve();
      await applying;
      source.free();
    }
  });

  it.each(["trim", "idle release", "reset"] as const)(
    "uses only the live document after %s during authority lookup",
    async (transition) => {
      const entered = Promise.withResolvers<void>();
      const resume = Promise.withResolvers<void>();
      let pause = false;
      const { room, sql, sockets } = makeRoom(new MemorySql(), undefined, async () => {
        if (pause) {
          pause = false;
          entered.resolve();
          await resume.promise;
        }
        return new Response(null, { status: 200 });
      });
      const socket = await join(room, "user-a", "authority-chat");
      const state = socket.deserializeAttachment() as JoinState;
      state.grantId = "grant-1";
      state.grantExpiresAt = Date.now() + 600_000;
      sockets.push(socket as unknown as WebSocket);
      socket.sent.length = 0;
      const internals = room as unknown as SessionRoomInternals;
      const source = new LoroDoc();
      source.getMap("metadata").set("baseline", true);
      expect((await room.fetch(authedRequest("/append", "user-a", {
        method: "POST",
        body: source.export({ mode: "update" })
      }))).status).toBe(200);
      await room.fetch(authedRequest("/stats", "user-a"));
      socket.sent.length = 0;
      const before = source.oplogVersion();
      source.getMap("metadata").set("duringAuthority", true);
      const delta = source.export({ mode: "update", from: before });
      before.free();
      pause = true;
      const applying = internals.applyUpdates(
        socket as unknown as WebSocket, state, CrdtType.Loro, "authority-chat",
        "0x0000000000000001", [delta]
      );
      try {
        await Promise.race([entered.promise, applying]);
        expect(pause).toBe(false);
        if (transition === "trim") {
          const live = await internals.ensureDoc();
          sql.meta.set("checkpoints", JSON.stringify([{
            at: Date.now() - 365 * 24 * 60 * 60 * 1000,
            frontiers: live.frontiers()
          }]));
          expect(await internals.trimHistoryIfDue(live, Date.now())).toBe(true);
        } else if (transition === "idle release") {
          vi.advanceTimersByTime(61_000);
        } else {
          expect((await room.fetch(authedRequest("/reset-log", "user-a", {
            method: "POST"
          }))).status).toBe(200);
          // Even a new live doc must not admit a pre-reset socket's write.
          await internals.ensureDoc();
        }
        resume.resolve();
        await applying;
        const messages = socket.sent.map((bytes) => decode(bytes));
        if (transition === "reset") {
          expect(socket.closed).toContainEqual({ code: 4410, reason: "room reset" });
          expect(messages).toEqual([]);
        } else {
          expect(messages).toEqual([{
            type: MessageType.Ack,
            crdt: CrdtType.Loro,
            roomId: "authority-chat",
            refId: "0x0000000000000001",
            status: transition === "trim" ? UpdateStatusCode.Ok : UpdateStatusCode.InvalidUpdate
          }]);
        }
        const live = await internals.ensureDoc();
        expect(live.getMap("metadata").get("duringAuthority")).toBe(
          transition === "trim" ? true : undefined
        );
        const snapshot = await room.fetch(authedRequest("/snapshot", "user-a"));
        const mirror = new LoroDoc();
        try {
          mirror.import(new Uint8Array(await snapshot.arrayBuffer()));
          expect(mirror.getMap("metadata").get("duringAuthority")).toBe(
            transition === "trim" ? true : undefined
          );
        } finally {
          mirror.free();
        }
      } finally {
        resume.resolve();
        await applying;
        source.free();
      }
    }
  );

  it.each(["trim", "idle release", "reset"] as const)(
    "answers a join safely after %s during authority lookup",
    async (transition) => {
      const entered = Promise.withResolvers<void>();
      const resume = Promise.withResolvers<void>();
      let pause = true;
      const { room, sql, sockets } = makeRoom(new MemorySql(), undefined, async () => {
        pause = false;
        entered.resolve();
        await resume.promise;
        return new Response(null, { status: 200 });
      });
      const source = new LoroDoc();
      source.getMap("metadata").set("baseline", true);
      sql.appendUpdate(source.export({ mode: "update" }));
      const internals = room as unknown as SessionRoomInternals;
      const socket = new CapturingSocket();
      const state: JoinState = {
        userId: "user-a",
        projectScope: PROJECT_SCOPE,
        capabilities: CAPABILITIES,
        rooms: [],
        grantId: "grant-1",
        grantExpiresAt: Date.now() + 600_000
      };
      socket.serializeAttachment(state);
      sockets.push(socket as unknown as WebSocket);
      const joining = internals.handleJoin(
        socket as unknown as WebSocket, state, joinRequest("authority-chat")
      );
      try {
        await Promise.race([entered.promise, joining]);
        expect(pause).toBe(false);
        if (transition === "trim") {
          const live = await internals.ensureDoc();
          sql.meta.set("checkpoints", JSON.stringify([{
            at: Date.now() - 365 * 24 * 60 * 60 * 1000,
            frontiers: live.frontiers()
          }]));
          expect(await internals.trimHistoryIfDue(live, Date.now())).toBe(true);
        } else if (transition === "idle release") {
          vi.advanceTimersByTime(61_000);
        } else {
          expect((await room.fetch(authedRequest("/reset-log", "user-a", {
            method: "POST"
          }))).status).toBe(200);
          await internals.ensureDoc();
        }
        resume.resolve();
        await joining;
        const messages = socket.sent.map((bytes) => decode(bytes));
        if (transition === "trim") {
          expect(messages[0]?.type).toBe(MessageType.JoinResponseOk);
          expect(state.rooms).toContain(CrdtType.Loro);
          const backfill = messages.find((message) => message.type === MessageType.DocUpdate);
          expect(backfill?.type).toBe(MessageType.DocUpdate);
          const mirror = new LoroDoc();
          try {
            if (backfill?.type === MessageType.DocUpdate) {
              for (const update of backfill.updates) mirror.import(update);
            }
            expect(mirror.getMap("metadata").get("baseline")).toBe(true);
          } finally {
            mirror.free();
          }
        } else {
          expect(state.rooms).toEqual([]);
          if (transition === "idle release") {
            expect(messages).toEqual([expect.objectContaining({
              type: MessageType.JoinError,
              code: JoinErrorCode.AppError
            })]);
          } else {
            expect(socket.closed).toContainEqual({ code: 4410, reason: "room reset" });
            expect(messages).toEqual([]);
          }
        }
      } finally {
        resume.resolve();
        await joining;
        source.free();
      }
    }
  );

  it("bounds fragment reservations across incomplete batches and rejects oversized payloads", async () => {
    const { room } = makeRoom();
    const socket = await join(room, "user-a", "fragment-chat");
    socket.sent.length = 0;
    const envelope = { crdt: CrdtType.Loro, roomId: "fragment-chat" };
    const send = async (message: ProtocolMessage) => {
      await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
    };
    const header = { type: MessageType.DocUpdateFragmentHeader, ...envelope } as const;
    await send({
      ...header,
      batchId: "0x0000000000000001",
      fragmentCount: 1025,
      totalSizeBytes: 1
    });
    await send({
      ...header,
      batchId: "0x0000000000000002",
      fragmentCount: 1,
      totalSizeBytes: 64 * 1024 * 1024 + 1
    });
    await send({
      ...header,
      batchId: "0x0000000000000003",
      fragmentCount: 1,
      totalSizeBytes: 64 * 1024 * 1024
    });
    await send({
      ...header,
      batchId: "0x0000000000000004",
      fragmentCount: 1,
      totalSizeBytes: 1
    });
    // Replacing an existing batch releases its previous reservation.
    await send({
      ...header,
      batchId: "0x0000000000000003",
      fragmentCount: 1,
      totalSizeBytes: 1
    });
    await send({
      type: MessageType.DocUpdateFragment,
      ...envelope,
      batchId: "0x0000000000000003",
      index: 0,
      fragment: new Uint8Array([1, 2])
    });
    expect(socket.sent.map((bytes) => decode(bytes))).toEqual(
      [1, 2, 4, 3].map((id) => ({
        type: MessageType.Ack,
        ...envelope,
        refId: `0x${id.toString(16).padStart(16, "0")}`,
        status: UpdateStatusCode.PayloadTooLarge
      }))
    );
  });

  it("persists oversized Loro updates and subsequent deltas across a cold restart", async () => {
    const { room, sql } = makeRoom();
    await join(room, "user-a", "large-workspace");
    const source = new LoroDoc();
    source.getMap("metadata").set("before", true);
    source.commit();
    const append = (bytes: Uint8Array) => room.fetch(
      authedRequest("/append", "user-a", { method: "POST", body: bytes })
    );
    expect((await append(source.export({ mode: "update" }))).status).toBe(200);
    expect((await room.fetch(authedRequest("/stats", "user-a"))).status).toBe(200);

    const payload = oversizedPayload();
    source.getMap("metadata").set("payload", payload);
    source.commit();
    const oversized = source.export({ mode: "update" });
    expect(oversized.byteLength).toBeGreaterThan(2_000_000);
    expect((await append(oversized)).status).toBe(200);
    expect((await room.fetch(authedRequest("/stats", "user-a"))).status).toBe(200);

    const beforeDelta = source.oplogVersion();
    source.getMap("metadata").set("after", true);
    source.commit();
    expect((await append(source.export({ mode: "update", from: beforeDelta }))).status).toBe(200);
    expect((await room.fetch(authedRequest("/stats", "user-a"))).status).toBe(200);

    const reopened = makeRoom(sql).room;
    const snapshot = await reopened.fetch(authedRequest("/snapshot", "user-a"));
    expect(snapshot.status).toBe(200);
    const restored = new LoroDoc();
    restored.import(new Uint8Array(await snapshot.arrayBuffer()));
    expect(restored.getMap("metadata").get("before")).toBe(true);
    const recoveredPayload = restored.getMap("metadata").get("payload");
    expect(recoveredPayload).toBeInstanceOf(Uint8Array);
    // Compare every byte without recursively inspecting millions of indices.
    expect(Buffer.compare(recoveredPayload as Uint8Array, payload)).toBe(0);
    expect(restored.getMap("metadata").get("after")).toBe(true);
    restored.free();
    source.free();
  });

  it("compacts oversized backfills without losing writes accepted during persistence", async () => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    const { room, sql } = makeRoom(new MemorySql(), async () => {
      if (!pause) return;
      pause = false;
      entered.resolve();
      await resume.promise;
    });
    await join(room, "user-a", "compacting-workspace");
    const source = new LoroDoc();
    source.getMap("metadata").set("payload", oversizedPayload());
    source.commit();
    source.getMap("metadata").set("payload", "latest");
    source.commit();
    const append = (bytes: Uint8Array) => room.fetch(
      authedRequest("/append", "user-a", { method: "POST", body: bytes })
    );
    expect((await append(source.export({ mode: "update" }))).status).toBe(200);
    pause = true;
    const flushing = room.fetch(authedRequest("/stats", "user-a"));
    try {
      await Promise.race([entered.promise, flushing]);
      expect(pause).toBe(false);
      const beforeDelta = source.oplogVersion();
      source.getMap("metadata").set("duringPersistence", true);
      source.commit();
      expect((await append(source.export({ mode: "update", from: beforeDelta }))).status).toBe(200);
      const overlappingFlush = room.fetch(authedRequest("/stats", "user-a"));
      resume.resolve();
      await Promise.all([flushing, overlappingFlush]);

      const live = await room.fetch(authedRequest("/snapshot", "user-a"));
      const bytes = new Uint8Array(await live.arrayBuffer());
      const mirror = new LoroDoc();
      mirror.import(bytes);
      expect(mirror.getMap("metadata").toJSON()).toEqual({
        payload: "latest",
        duringPersistence: true
      });
      mirror.free();

      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      const recovered = new LoroDoc();
      recovered.import(new Uint8Array(await cold.arrayBuffer()));
      expect(recovered.getMap("metadata").toJSON()).toEqual({
        payload: "latest",
        duringPersistence: true
      });
      recovered.free();
    } finally {
      resume.resolve();
      await flushing;
      source.free();
    }
  });

  it("recovers workspace discovery after repeated replay failures without resetting history", async () => {
    const source = new LoroDoc();
    const mirror = new LoroDoc();
    const sql = new MemorySql();
    let version: VersionVector | undefined;
    try {
      source.getMap("devices").set("host", { id: "host", name: "Desktop", platform: "macos" });
      source.getMap("chats").set("chat", { id: "chat", deviceId: "host", title: "Existing session" });
      source.commit();
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      version = source.oplogVersion();
      source.getMap("sessionRefs").set("6:user-a:chat", { chatId: "chat", userId: "user-a", addedAt: 1 });
      source.commit();
      sql.appendUpdate(source.export({ mode: "update", from: version }));
      sql.meta.set("chatId", "ws4/project-a");
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("replayAttempts", "3");

      const socket = await join(makeRoom(sql).room, "user-a", "ws4/project-a");
      for (const bytes of socket.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) mirror.importBatch(message.updates);
      }
      expect(mirror.toJSON()).toEqual(source.toJSON());
      expect(sql.meta.get("replayAttempts")).toBe("0");
      expect(sql.hasBlob("snapshot")).toBe(true);
      expect(sql.updateCount()).toBe(1);
    } finally {
      version?.free(); mirror.free(); source.free();
    }
  });

  it("preserves rejected workspace history through replay failures and cold restart", async () => {
    const corruptSnapshot = new Uint8Array(
      readFileSync(
        fileURLToPath(new NodeUrl("./fixtures/corrupt-loro-snapshot.bin", import.meta.url))
      )
    );

    for (const source of ["snapshot", "update"] as const) {
      const sql = new MemorySql();
      if (source === "snapshot") sql.putBlob("snapshot", corruptSnapshot);
      else sql.appendUpdate(corruptSnapshot);
      const { room, sockets } = makeRoom(sql);
      const socket = new CapturingSocket();
      sockets.push(socket as unknown as WebSocket);
      const internals = room as unknown as SessionRoomInternals;

      await expect(internals.ensureDoc()).rejects.toBeDefined();
      expect(socket.closed).toContainEqual({ code: 4410, reason: "room reset" });
      await expect((makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).rejects.toBeDefined();
      expect(sql.hasBlob("snapshot")).toBe(source === "snapshot");
      expect(sql.updateCount()).toBe(source === "update" ? 1 : 0);
      const stored = Array.from(sql.exec(source === "snapshot" ? "SELECT bytes FROM blobs WHERE name = ?" : "SELECT bytes FROM updates ORDER BY seq", "snapshot"));
      expect(Buffer.compare(Buffer.from(stored[0].bytes as ArrayBuffer), corruptSnapshot)).toBe(0);
      sql.meta.set("replayAttempts", "3");
      await expect((makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).rejects.toBeDefined();
      const retained = Array.from(sql.exec(source === "snapshot" ? "SELECT bytes FROM blobs WHERE name = ?" : "SELECT bytes FROM updates ORDER BY seq", "snapshot"));
      expect(Buffer.compare(Buffer.from(retained[0].bytes as ArrayBuffer), corruptSnapshot)).toBe(0);
    }
  });

  it("lets different authenticated users mutate and read every authorized chat surface", async () => {
    const { room } = makeRoom();
    const firstWrite = await room.fetch(
      authedRequest("/diff", "user-a", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ revision: "from-a" })
      })
    );
    expect(firstWrite.status).toBe(200);

    const firstRead = await room.fetch(authedRequest("/diff", "user-b"));
    expect(firstRead.status).toBe(200);
    await expect(firstRead.json()).resolves.toEqual({ revision: "from-a" });

    const secondWrite = await room.fetch(
      authedRequest("/diff", "user-b", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ revision: "from-b" })
      })
    );
    expect(secondWrite.status).toBe(200);

    const secondRead = await room.fetch(authedRequest("/diff", "user-a"));
    await expect(secondRead.json()).resolves.toEqual({ revision: "from-b" });

    expect((await room.fetch(authedRequest("/stats", "user-b"))).status).toBe(200);
    expect((await room.fetch(authedRequest("/tail", "user-b"))).status).toBe(200);
    expect((await room.fetch(authedRequest("/snapshot", "user-b"))).status).toBe(200);
    expect(
      (
        await room.fetch(
          authedRequest("/append", "user-b", {
            method: "POST",
            body: new Uint8Array()
          })
        )
      ).status
    ).toBe(200);
  });

  it("rejects unauthenticated requests before every routed chat handler", async () => {
    const { room } = makeRoom();
    const requests = [
      new Request("https://room.test/ws"),
      new Request("https://room.test/stats"),
      new Request("https://room.test/tail"),
      new Request("https://room.test/diff"),
      new Request("https://room.test/diff", { method: "POST", body: "{}" }),
      new Request("https://room.test/snapshot"),
      new Request("https://room.test/append", { method: "POST", body: new Uint8Array() })
    ];

    for (const request of requests) {
      expect((await room.fetch(request)).status).toBe(401);
    }
  });

  it("claims an empty room for the verified project on first join", async () => {
    const { room, sql } = makeRoom();

    const emptyTail = await room.fetch(authedRequest("/tail", "user-a"));
    expect(emptyTail.status).toBe(404);
    await join(room, "user-a", "new-shared-chat");

    expect(sql.meta.get("chatId")).toBe("new-shared-chat");
    expect(sql.meta.get("owner")).toBe(PROJECT_SCOPE);
  });
});

describe("session room identifiers", () => {
  it("canonicalizes uppercase UUIDs to the sole global room name", () => {
    expect(canonicalSessionId("AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE")).toBe(
      "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
    );
  });

  it("rejects non-UUID session aliases", () => {
    expect(canonicalSessionId("not-a-session-uuid")).toBeUndefined();
  });
});
