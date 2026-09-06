import { Buffer } from "node:buffer";
import { readFileSync } from "node:fs";
import { fileURLToPath, URL as NodeUrl } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CrdtType, JoinErrorCode, MessageType, UpdateStatusCode, decode, encode, type JoinRequest, type ProtocolMessage } from "loro-protocol";
import { LoroDoc } from "loro-crdt";
import {
  AUTH_CAPABILITIES_HEADER,
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  type Env
} from "./env";
import { canonicalSessionId, SessionRoom } from "./session-room";

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
  grantStatus: () => Promise<Response> = async () => new Response(null, { status: 200 })
): { room: SessionRoom; sql: MemorySql; sockets: WebSocket[] } => {
  const sockets: WebSocket[] = [];
  const storage = {
    sql: sql as unknown as SqlStorage,
    sync,
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
  version = new Uint8Array()
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
          // The unchanged retained text must not be retransmitted to a caught-up
          // client. This also rejects an unconditional full-snapshot fallback.
          const bytes = backfill.updates.reduce((total, update) => total + update.byteLength, 0);
          expect(bytes).toBeLessThan(coveredSnapshot.byteLength / 2);
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
      const second = {
        type: MessageType.DocUpdateFragment as const,
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
    const header = { type: MessageType.DocUpdateFragmentHeader as const, ...envelope };
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
      // Only the current small value belongs in the backfill, not the deleted
      // multi-megabyte history that triggered compaction.
      expect(bytes.byteLength).toBeLessThan(100_000);
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

  it("quarantines incident-shaped persisted Loro state and forces a clean reconnect", async () => {
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
      expect(sql.hasBlob("snapshot")).toBe(false);
      expect(sql.updateCount()).toBe(0);
      expect(sql.meta.get("postReset")).toBe("1");
      expect(sql.meta.get("replayAttempts")).toBe("0");
      expect(socket.closed).toContainEqual({ code: 4410, reason: "room reset" });

      const clean = await internals.ensureDoc();
      clean.getMap("after").set("usable", true);
      expect(clean.getMap("after").get("usable")).toBe(true);
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
