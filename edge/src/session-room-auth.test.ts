import { Buffer } from "node:buffer";
import { readFileSync } from "node:fs";
import { fileURLToPath, URL as NodeUrl } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CrdtType, JoinErrorCode, MessageType, UpdateStatusCode, decode, encode, type JoinRequest, type ProtocolMessage } from "loro-protocol";
import { LoroDoc, LoroMap, LoroList, LoroText, decodeImportBlobMeta } from "loro-crdt";
import type { VersionVector } from "loro-crdt";
import {
  AUTH_CAPABILITIES_HEADER,
  AUTH_GRANT_HEADER,
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  GRANT_EVENT_HEADER,
  ROOM_KIND_HEADER,
  DURABLE_SYNC_PROTOCOL,
  SYNC_PROTOCOL_HEADER,
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
  transactionSync<T>(body: () => T): T {
    const meta = new Map(this.meta);
    const blobs = new Map([...this.blobs].map(([name, chunks]) => [name, new Map(chunks)]));
    const updates = this.updates.slice();
    try { return body(); }
    catch (error) {
      this.meta.clear();
      for (const [key, value] of meta) this.meta.set(key, value);
      this.blobs.clear();
      for (const [name, chunks] of blobs) this.blobs.set(name, chunks);
      this.updates.splice(0, this.updates.length, ...updates);
      throw error;
    }
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
  durableSync?: boolean;
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
  relay(from: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): Promise<void>;
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
  environment: Env["ENVIRONMENT"] = "local",
  roomId = "room-a"
): { room: SessionRoom; sql: MemorySql; sockets: WebSocket[] } => {
  const sockets: WebSocket[] = [];
  const storage = {
    sql: sql as unknown as SqlStorage,
    sync,
    transactionSync: <T>(body: () => T): T => sql.transactionSync(body),
    getAlarm: async () => null,
    setAlarm: async () => {}
  };
  const ctx = {
    id: { toString: () => roomId },
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
  if (!headers.has(SYNC_PROTOCOL_HEADER)) headers.set(SYNC_PROTOCOL_HEADER, DURABLE_SYNC_PROTOCOL);
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
    durableSync: true,
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

  it("preserves ephemeral membership when a concurrent document join finishes cold replay", async () => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let cold = true;
    const { room } = makeRoom(new MemorySql(), async () => {
      if (!cold) return;
      cold = false; entered.resolve(); await resume.promise;
    });
    const socket = new CapturingSocket();
    const state: JoinState = { userId: "user-a", projectScope: PROJECT_SCOPE,
      capabilities: CAPABILITIES, durableSync: true, rooms: [] };
    socket.serializeAttachment(structuredClone(state));
    const internals = room as unknown as SessionRoomInternals;
    const joining = internals.handleJoin(socket as unknown as WebSocket, state, joinRequest("room-a"));
    try {
      await entered.promise;
      // Real hibernation attachments deserialize to independent values.
      await internals.handleJoin(socket as unknown as WebSocket,
        structuredClone(socket.deserializeAttachment()) as JoinState,
        { ...joinRequest("room-a"), crdt: CrdtType.LoroEphemeralStore });
      resume.resolve(); await joining;
      expect((socket.deserializeAttachment() as JoinState).rooms).toEqual(
        expect.arrayContaining([CrdtType.Loro, CrdtType.LoroEphemeralStore]));
      socket.sent.length = 0;
      await room.webSocketMessage(socket as unknown as WebSocket, encode({
        type: MessageType.DocUpdate, crdt: CrdtType.LoroEphemeralStore,
        roomId: "room-a", batchId: "0x0000000000000001", updates: []
      }).buffer as ArrayBuffer);
      expect(socket.sent.map((bytes) => decode(bytes))).toContainEqual(
        expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
    } finally { resume.resolve(); await joining; }
  });

  it.each(["disconnect", "revoke"] as const)("relays durable writes when the publisher loses authority via %s before ACK", async (loss) => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    let revoked = false;
    const sql = new MemorySql();
    sql.meta.set("roomKind", "workspace");
    const { room, sockets } = makeRoom(sql, async () => {
      if (!pause) return;
      pause = false; entered.resolve(); await resume.promise;
    }, async () => new Response(null, { status: revoked ? 403 : 200 }));
    const source = new LoroDoc();
    const mirror = new LoroDoc();
    let writing: Promise<void> | undefined;
    try {
      source.getMap("metadata").set("before", "accepted"); source.commit();
      const baseline = source.export({ mode: "snapshot" });
      sql.putBlob("snapshot", baseline); mirror.import(baseline);
      const publisher = await join(room, "user-a", "room-a");
      const reader = await join(room, "user-a", "room-a");
      sockets.push(publisher as unknown as WebSocket, reader as unknown as WebSocket);
      const state = publisher.deserializeAttachment() as JoinState;
      state.grantId = "grant-1"; state.grantExpiresAt = Date.now() + 600_000;
      publisher.serializeAttachment(state);
      const from = source.oplogVersion();
      let delta: Uint8Array;
      try {
        source.getMap("metadata").set("live", "must reach readers"); source.commit();
        delta = source.export({ mode: "update", from });
      } finally { from.free(); }
      publisher.sent.length = 0; reader.sent.length = 0; pause = true;
      writing = (room as unknown as SessionRoomInternals).applyUpdates(publisher as unknown as WebSocket,
        state, CrdtType.Loro, "room-a", "0x0000000000000001", [delta]);
      await entered.promise;
      if (loss === "disconnect") publisher.close(); else revoked = true;
      resume.resolve(); await writing;
      expect(publisher.sent.map((bytes) => decode(bytes))).not.toContainEqual(
        expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      for (const bytes of reader.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) mirror.importBatch(message.updates);
      }
      expect(mirror.toJSON()).toEqual(source.toJSON());
      expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(source.toJSON());
    } finally { resume.resolve(); await writing; source.free(); mirror.free(); }
  });

  it("delivers live updates without waiting for another reader's grant authority", async () => {
    const authority = Promise.withResolvers<Response>();
    const entered = Promise.withResolvers<void>();
    const delivered = Promise.withResolvers<void>();
    const { room, sockets } = makeRoom(new MemorySql(), async () => {}, async () => {
      entered.resolve(); return authority.promise;
    });
    const publisher = await join(room, "user-a", "room-a");
    const slow = await join(room, "user-a", "room-a");
    const reader = await join(room, "user-a", "room-a");
    const state = slow.deserializeAttachment() as JoinState;
    state.grantId = "grant-1"; state.grantExpiresAt = Date.now() + 600_000;
    slow.serializeAttachment(state);
    sockets.push(slow as unknown as WebSocket, reader as unknown as WebSocket);
    slow.sent.length = 0; reader.sent.length = 0;
    vi.spyOn(reader, "send").mockImplementation((bytes) => {
      reader.sent.push(bytes); delivered.resolve();
    });
    const source = new LoroDoc();
    const mirror = new LoroDoc();
    source.getText("t").insert(0, "live for the other reader"); source.commit();
    const sending = (room as unknown as SessionRoomInternals).relay(publisher as unknown as WebSocket,
      CrdtType.Loro, "room-a", [source.export({ mode: "snapshot" })]);
    try {
      await entered.promise;
      await delivered.promise; // Must resolve BEFORE the stalled authority does.
      for (const bytes of reader.sent) {
        const message = decode(bytes);
        if (message.type === MessageType.DocUpdate) mirror.importBatch(message.updates);
      }
      expect(mirror.toJSON()).toEqual(source.toJSON());
      authority.resolve(new Response(null, { status: 403 })); await sending;
      expect(slow.sent).toHaveLength(0);
      expect(slow.closed).toContainEqual({ code: 4403, reason: "device grant invalid" });
    } finally {
      authority.resolve(new Response(null, { status: 403 })); await sending;
      source.free(); mirror.free();
    }
  });

  it.each(["", "?syncProtocol=unknown", "?syncProtocol=durable-records-v1&syncProtocol=unknown"])(
    "keeps authenticated legacy rooms read-only despite spoofed headers (%s)", async (query) => {
      const NativeResponse = Response;
      vi.stubGlobal("Response", class extends NativeResponse {
        constructor(body?: BodyInit | null, init?: ResponseInit) {
          super(body, init?.status === 101 ? { ...init, status: 200 } : init);
          if (init?.status === 101) Object.defineProperty(this, "status", { value: 101 });
        }
      });
      vi.stubGlobal("WebSocketPair", class { 0 = new CapturingSocket(); 1 = new CapturingSocket(); });
      const { room, sql, sockets } = makeRoom();
      const source = new LoroDoc(); const reader = new LoroDoc();
      const sessionId = "11111111-1111-4111-8111-111111111111";
      const scope = "ashler-local";
      const env = {
        AUTH_MODE: "dev", ENVIRONMENT: "local", SCAFFOLD_PROJECT_SCOPE: scope,
        SCAFFOLD_CONTROL_PLANE_URL: "http://127.0.0.1:8788",
        SCAFFOLD_REQUIRED_CAPABILITIES: CAPABILITIES.join(" "),
        SESSION_ROOMS: { idFromName: (name: string) => name, get: () => ({ fetch: (request: Request) => room.fetch(request) }) }
      } as unknown as Env;
      const request = (path: string, init: RequestInit = {}) => worker.fetch(new Request(`http://127.0.0.1${path}`, {
        ...init, headers: { authorization: `Bearer alice@${scope}`, upgrade: "websocket",
          [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL }
      }), env);
      const send = (socket: CapturingSocket, message: ProtocolMessage) => room.webSocketMessage(
        socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
      try {
        source.getMap("metadata").set("accepted", true); source.commit();
        sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
        expect((await request(`/session/${sessionId}/ws${query}`)).status).toBe(101);
        const legacy = sockets[0] as unknown as CapturingSocket;
        await send(legacy, joinRequest(sessionId));
        expect(legacy.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.JoinResponseOk, permission: "read" }));
        for (const bytes of legacy.sent) {
          const message = decode(bytes);
          if (message.type === MessageType.DocUpdate) for (const update of message.updates) reader.import(update);
        }
        expect(reader.toJSON()).toEqual(source.toJSON());
        const accepted = source.toJSON();
        source.getMap("metadata").set("writer", true); source.commit();
        const update = source.export({ mode: "snapshot" });
        const envelope = { crdt: CrdtType.Loro, roomId: sessionId, batchId: "0x0000000000000001" as const };
        legacy.sent.length = 0;
        await send(legacy, { type: MessageType.DocUpdate, ...envelope, updates: [update] });
        await send(legacy, { type: MessageType.DocUpdateFragmentHeader, ...envelope, fragmentCount: 1, totalSizeBytes: update.length });
        await send(legacy, { type: MessageType.DocUpdateFragment, ...envelope, index: 0, fragment: update });
        expect(legacy.sent.map((bytes) => decode(bytes))).toEqual(Array.from({ length: 3 }, () => expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.PermissionDenied })));
        for (const path of [`/append/${sessionId}`, `/diff/${sessionId}`, `/workspace/${scope}/reset-log`]) {
          expect((await request(path + query, { method: "POST", body: update })).status).toBe(426);
        }
        expect(sql.updateCount()).toBe(0);
        expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(accepted);
        expect((await request(`/session/${sessionId}/ws?syncProtocol=${DURABLE_SYNC_PROTOCOL}`)).status).toBe(101);
        const writer = sockets[1] as unknown as CapturingSocket;
        await send(writer, joinRequest(sessionId));
        expect(writer.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.JoinResponseOk, permission: "write" }));
        writer.sent.length = 0;
        await send(writer, { type: MessageType.DocUpdate, ...envelope, updates: [update] });
        expect(writer.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
        expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(source.toJSON());
        // Pre-rollout attachments retain capabilities/membership, not write permission.
        const state = writer.deserializeAttachment() as JoinState;
        delete state.durableSync; writer.serializeAttachment(state); writer.sent.length = 0;
        source.getMap("metadata").set("staleSocket", true); source.commit();
        await send(writer, { type: MessageType.DocUpdate, ...envelope, updates: [source.export({ mode: "snapshot" })] });
        expect(writer.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.PermissionDenied }));
        expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).not.toEqual(source.toJSON());
      } finally { source.free(); reader.free(); }
    }
  );

  it.each(["delta", "snapshot"] as const)("reconciles legacy re-appended accepted messages on the raw %s receiver boundary", async (mode) => {
    const source = new LoroDoc(); const publisher = new LoroDoc(); const observer = new LoroDoc();
    const sql = new MemorySql(); sql.meta.set("owner", PROJECT_SCOPE); sql.meta.set("chatId", "room-a");
    const appendMessage = (doc: LoroDoc, createdAt: number, text = "accepted original", deviceId = "owner-device") => {
      const messages = doc.getList("messages");
      const row = messages.insertContainer(0, new LoroMap());
      const parts = row.setContainer("parts", new LoroList());
      const part = parts.pushContainer(new LoroMap());
      const body = part.setContainer("text", new LoroText());
      try {
        for (const [key, value] of Object.entries({ id: "message-1", role: "user", createdAt, deviceId, status: "complete" })) row.set(key, value);
        part.set("id", "t0"); part.set("kind", "text"); body.insert(0, text); doc.commit();
      } finally { body.free(); part.free(); parts.free(); row.free(); messages.free(); }
    };
    try {
      appendMessage(source, 10);
      const commands = source.getList("commands"); const command = commands.pushContainer(new LoroMap());
      try {
        command.set("id", "command-1"); command.set("status", "applied"); command.set("issuedBy", "controller-device");
        command.set("kind", "run");
        command.set("payload", { kind: "run", messageId: "message-1", request: { prompt: "accepted original" } }); source.commit();
      } finally { command.free(); commands.free(); }
      const accepted = source.toJSON(); const baseline = source.export({ mode: "snapshot" });
      sql.putBlob("snapshot", baseline); publisher.import(baseline); observer.import(baseline);
      // Both processes cold-start. The old publisher appends the same immutable
      // message under new CRDT ancestry before its recovered observer rejoins.
      const { room, sockets } = makeRoom(sql);
      const sender = await join(room, "user-a", "room-a");
      const receiver = await join(room, "user-b", "room-a");
      sockets.push(sender as unknown as WebSocket, receiver as unknown as WebSocket);
      sender.sent.length = 0; receiver.sent.length = 0;
      const before = publisher.oplogVersion();
      let update: Uint8Array;
      let duplicateSnapshot: Uint8Array;
      try {
        appendMessage(publisher, 20); // Sorted before the originally accepted row.
        update = publisher.export(mode === "delta" ? { mode: "update", from: before } : { mode: "snapshot" });
        duplicateSnapshot = publisher.export({ mode: "snapshot" });
      } finally { before.free(); }
      const internals = room as unknown as SessionRoomInternals;
      await internals.applyUpdates(sender as unknown as WebSocket, sender.deserializeAttachment() as JoinState,
        CrdtType.Loro, "room-a", "0x0000000000000001", [update]);
      expect(sender.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      for (const [socket, client] of [[sender, publisher], [receiver, observer]] as const) {
        for (const bytes of socket.sent) {
          const message = decode(bytes);
          if (message.type === MessageType.DocUpdate) client.importBatch(message.updates);
        }
        // This is the raw doc handed to either old or native WatchDocMessages,
        // not a deduplicated tail/UI projection. Command identity/outcome stays.
        expect(client.toJSON()).toEqual(accepted);
      }
      expect((await internals.ensureDoc()).toJSON()).toEqual(accepted);
      expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(accepted);
      const rows = sql.updateCount();
      // Repeating the unconsumed batch cannot create another accepted row.
      await internals.applyUpdates(sender as unknown as WebSocket, sender.deserializeAttachment() as JoinState,
        CrdtType.Loro, "room-a", "0x0000000000000001", [update]);
      expect(sql.updateCount()).toBe(rows);
      for (const [text, device] of [["conflicting accepted content", "owner-device"], ["accepted original", "foreign-device"]]) {
        const conflict = new LoroDoc();
        try {
          conflict.import(publisher.export({ mode: "snapshot" })); appendMessage(conflict, 30, text, device);
          expect((await room.fetch(authedRequest("/append", "user-a", { method: "POST", body: conflict.export({ mode: "snapshot" }) }))).status).toBe(400);
          expect((await internals.ensureDoc()).toJSON()).toEqual(accepted);
          expect(sql.updateCount()).toBe(rows); expect(sql.hasBlob("snapshot")).toBe(true);
        } finally { conflict.free(); }
      }
      // Cold legacy log replay retains its baseline's authoritative metadata.
      const legacy = new MemorySql(); legacy.putBlob("snapshot", baseline); legacy.appendUpdate(update);
      expect((await (makeRoom(legacy).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(accepted);
      // An already-ambiguous snapshot cannot self-authorize choosing a timestamp.
      const ambiguous = new MemorySql(); ambiguous.putBlob("snapshot", duplicateSnapshot);
      await expect((makeRoom(ambiguous).room as unknown as SessionRoomInternals).ensureDoc()).rejects.toThrow();
      expect(ambiguous.hasBlob("snapshot")).toBe(true); expect(ambiguous.updateCount()).toBe(0);
      // An imported applied outcome is not an acceptance anchor either.
      const asserted = new LoroDoc();
      try {
        asserted.import(baseline);
        const ledger = asserted.getList("commands"); const pending = ledger.get(0) as LoroMap;
        try { pending.set("status", "pending"); asserted.commit(); } finally { pending.free(); ledger.free(); }
        const untrusted = new MemorySql(); untrusted.meta.set("owner", PROJECT_SCOPE);
        untrusted.putBlob("snapshot", asserted.export({ mode: "snapshot" }));
        const beforeAssertion = asserted.toJSON();
        const untrustedRoom = makeRoom(untrusted).room;
        const forgedLedger = asserted.getList("commands"); const forged = forgedLedger.get(0) as LoroMap;
        try { forged.set("status", "applied"); appendMessage(asserted, 20); } finally { forged.free(); forgedLedger.free(); }
        expect((await untrustedRoom.fetch(authedRequest("/append", "user-a", { method: "POST", body: asserted.export({ mode: "snapshot" }) }))).status).toBe(400);
        expect((await (untrustedRoom as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(beforeAssertion);
        expect(untrusted.updateCount()).toBe(0); expect(untrusted.hasBlob("snapshot")).toBe(true);
      } finally { asserted.free(); }
    } finally { observer.free(); publisher.free(); source.free(); }
  });

  it("preserves a legacy positional writer's complete result across deferred repair and Edge restart", async () => {
    const publisher = new LoroDoc(); const sql = new MemorySql();
    sql.meta.set("owner", PROJECT_SCOPE); sql.meta.set("chatId", "room-a");
    const appendEntry = (id: string, role: string, status: string, createdAt: number, text: string) => {
      const messages = publisher.getList("messages"); const index = messages.length;
      const row = messages.pushContainer(new LoroMap()); const parts = row.setContainer("parts", new LoroList());
      const part = parts.pushContainer(new LoroMap()); const body = part.setContainer("text", new LoroText());
      try {
        for (const [key, value] of Object.entries({ id, role, status, createdAt, deviceId: "owner-device" })) row.set(key, value);
        part.set("id", "t0"); part.set("kind", "text"); body.insert(0, text); publisher.commit();
        return index;
      } finally { body.free(); part.free(); parts.free(); row.free(); messages.free(); }
    };
    const publishOwner = (status: string, at: number) => {
      const publications = publisher.getList("publications"); const row = publications.pushContainer(new LoroMap());
      try {
        row.set("id", `owner-${at}`);
        row.set("record", { publishedBy: "user-a", value: { kind: "agentSession", value: {
          sessionId: "room-a", ownerSubject: "user-a", ownerDeviceId: "owner-device", status, createdAt: 10, updatedAt: at
        } } }); publisher.commit();
      } finally { row.free(); publications.free(); }
    };
    try {
      appendEntry("message-1", "user", "complete", 10, "accepted original");
      const commands = publisher.getList("commands"); const command = commands.pushContainer(new LoroMap());
      try {
        command.set("id", "command-1"); command.set("kind", "run"); command.set("status", "applied");
        command.set("payload", { kind: "run", messageId: "message-1", request: { prompt: "accepted original" } }); publisher.commit();
      } finally { command.free(); commands.free(); }
      publishOwner("working", 10); sql.putBlob("snapshot", publisher.export({ mode: "snapshot" }));
      let room = makeRoom(sql).room; let sender = await join(room, "user-a", "room-a");
      let internals = room as unknown as SessionRoomInternals;
      const publish = async (mutate: () => void) => {
        const before = publisher.oplogVersion();
        try {
          mutate(); sender.sent.length = 0;
          await internals.applyUpdates(sender as unknown as WebSocket, sender.deserializeAttachment() as JoinState,
            CrdtType.Loro, "room-a", "0x0000000000000001", [publisher.export({ mode: "update", from: before })]);
          expect(sender.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
          for (const bytes of sender.sent) { const message = decode(bytes); if (message.type === MessageType.DocUpdate) publisher.importBatch(message.updates); }
        } finally { before.free(); }
      };
      // Working precedes the first stream row: deleting the duplicate now could
      // race a positional writer that has already begun locally but not synced.
      await publish(() => { appendEntry("message-1", "user", "complete", 20, "accepted original"); });
      expect(publisher.toJSON().messages.filter((row: { id: string }) => row.id === "message-1")).toHaveLength(2);
      room = makeRoom(sql).room; internals = room as unknown as SessionRoomInternals;
      sender = await join(room, "user-a", "room-a");
      let writerIndex = -1;
      await publish(() => { writerIndex = appendEntry("answer-1", "assistant", "streaming", 21, "first"); });
      const writeAtOldIndex = (suffix: string, status: string) => {
        const messages = publisher.getList("messages"); const row = messages.get(writerIndex) as LoroMap;
        const parts = row.get("parts") as LoroList; const part = parts.get(0) as LoroMap; const body = part.get("text") as LoroText;
        try {
          expect(row.get("id")).toBe("answer-1"); body.insert(body.length, suffix); row.set("status", status); publisher.commit();
        } finally { body.free(); part.free(); parts.free(); row.free(); messages.free(); }
      };
      await publish(() => { writeAtOldIndex(" + middle", "streaming"); });
      await publish(() => { writeAtOldIndex(" + final result", "complete"); });
      expect(publisher.toJSON().messages.filter((row: { id: string }) => row.id === "message-1")).toHaveLength(2);
      // Genuine terminal owner state, not an elapsed deadline, retires the
      // position only after the old writer finished every suffix and its status.
      await publish(() => { publishOwner("idle", 30); });
      const expected = publisher.toJSON();
      expect(expected.messages.filter((row: { id: string }) => row.id === "message-1")).toHaveLength(1);
      expect(expected.messages.find((row: { id: string }) => row.id === "message-1").createdAt).toBe(10);
      expect(expected.messages.find((row: { id: string }) => row.id === "answer-1")).toMatchObject({ status: "complete", parts: [{ kind: "text", text: "first + middle + final result" }] });
      expect((await internals.ensureDoc()).toJSON()).toEqual(expected);
      expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(expected);
    } finally { publisher.free(); }
  });

  it.each(["delta", "snapshot"] as const)("durably acknowledges a %s before publisher and edge restart, without duplicate admission", async (mode) => {
    const sql = new MemorySql();
    sql.meta.set("roomKind", "workspace");
    sql.meta.set("owner", PROJECT_SCOPE);
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    const { room } = makeRoom(sql, async () => {
      if (!pause) return;
      pause = false;
      entered.resolve();
      await resume.promise;
    });
    const source = new LoroDoc();
    const commands = source.getMap("commands");
    const admissions = source.getList("admissions");
    const metadata = source.getMap("metadata");
    const mirror = new LoroDoc();
    let writing: Promise<void> | undefined;
    let publisherFreed = false;
    try {
      metadata.set("removed", true);
      source.commit();
      sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
      const socket = await join(room, "user-a", "ws4/project-a");
      const before = source.oplogVersion();
      let bytes: Uint8Array;
      try {
        commands.set("command-1", { id: "command-1", expiresAt: Date.now() + 60_000, owner: "user-a", status: "accepted" });
        admissions.push("command-1");
        metadata.delete("removed");
        source.commit();
        bytes = mode === "snapshot" ? source.export({ mode: "snapshot" }) : source.export({ mode: "update", from: before });
      } finally { before.free(); }
      const expected = source.toJSON();
      socket.sent.length = 0;
      pause = true;
      writing = (room as unknown as SessionRoomInternals).applyUpdates(
        socket as unknown as WebSocket, socket.deserializeAttachment() as JoinState,
        CrdtType.Loro, "ws4/project-a", "0x0000000000000001", [bytes]
      );
      await entered.promise;
      expect(socket.sent.map((packet) => decode(packet))).not.toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      // Publisher no longer holds a live CRDT; accepted bytes must stand alone.
      commands.free(); admissions.free(); metadata.free(); source.free();
      publisherFreed = true;
      resume.resolve();
      await writing;
      expect(socket.sent.map((packet) => decode(packet))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      socket.sent.length = 0; // ACK lost to the publisher.
      const restarted = makeRoom(sql).room;
      const internals = restarted as unknown as SessionRoomInternals;
      expect((await internals.ensureDoc()).toJSON()).toEqual(expected);
      const retried = await join(restarted, "user-a", "ws4/project-a");
      const rows = sql.updateCount();
      await internals.applyUpdates(retried as unknown as WebSocket, retried.deserializeAttachment() as JoinState,
        CrdtType.Loro, "ws4/project-a", "0x0000000000000001", [bytes]);
      expect((await internals.ensureDoc()).toJSON()).toEqual(expected);
      expect(sql.updateCount()).toBe(rows);
      mirror.import((await internals.ensureDoc()).export({ mode: "snapshot" }));
      expect(mirror.toJSON()).toEqual(expected);
    } finally {
      resume.resolve(); await writing;
      if (!publisherFreed) { commands.free(); admissions.free(); metadata.free(); source.free(); }
      mirror.free();
    }
  });

  it.each(["covered", "ahead"] as const)("preserves offline ancestry when a newer shallow repair has a %s floor", async (floorCoverage) => {
    const source = new LoroDoc();
    const offline = new LoroDoc();
    const mirror = new LoroDoc();
    const metadata = source.getMap("metadata");
    const offlineMetadata = offline.getMap("metadata");
    try {
      metadata.set("base", "accepted"); metadata.set("removed", true); source.commit();
      const initial = source.export({ mode: "snapshot" });
      const initialState = source.toJSON();
      offline.import(initial);
      const offlineBase = offline.oplogVersion();
      let offlineDelta: Uint8Array;
      try {
        offlineMetadata.set("offline", "retained"); offline.commit();
        offlineDelta = offline.export({ mode: "update", from: offlineBase });
      } finally { offlineBase.free(); }
      metadata.delete("removed"); metadata.set("beforeFloor", true); source.commit();
      const floor = source.frontiers();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace"); sql.meta.set("owner", PROJECT_SCOPE);
      sql.putBlob("snapshot", floorCoverage === "covered" ? source.export({ mode: "snapshot" }) : initial);
      const { room } = makeRoom(sql);
      const append = (bytes: Uint8Array) => room.fetch(authedRequest("/append", "user-a", { method: "POST", body: bytes }));
      metadata.set("clientAdded", true); source.commit();
      const shallow = source.export({ mode: "shallow-snapshot", frontiers: floor });
      const response = await append(shallow);
      if (floorCoverage === "ahead") {
        expect(response.status).toBe(400);
        expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(initialState);
        // A genuinely complete repair heals; newer metadata alone never does.
        expect((await append(source.export({ mode: "snapshot" }))).status).toBe(200);
      } else expect(response.status).toBe(200);
      expect((await append(offlineDelta)).status).toBe(200);
      source.import(offlineDelta);
      const recovered = await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc();
      mirror.import(recovered.export({ mode: "snapshot" }));
      expect(mirror.toJSON()).toEqual({ metadata: { base: "accepted", beforeFloor: true, clientAdded: true, offline: "retained" } });
      const expected = source.oplogVersion();
      const actual = recovered.oplogVersion();
      try { expect(actual.compare(expected)).toBe(0); }
      finally { actual.free(); expected.free(); }
    } finally { offlineMetadata.free(); metadata.free(); mirror.free(); offline.free(); source.free(); }
  });

  it.each(["empty", "retained pending"] as const)("handles a complete shallow bootstrap into a %s workspace without losing accepted ancestry", async (serverState) => {
    const source = new LoroDoc();
    const metadata = source.getMap("metadata");
    const mirror = new LoroDoc();
    try {
      metadata.set("base", "preserved"); source.commit();
      const before = source.oplogVersion();
      let pending: Uint8Array;
      try {
        metadata.set("record", { id: "command-1", status: "accepted" }); source.commit();
        pending = source.export({ mode: "update", from: before });
      } finally { before.free(); }
      const floor = source.frontiers();
      metadata.set("latest", true); source.commit();
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace"); sql.meta.set("owner", PROJECT_SCOPE);
      if (serverState === "retained pending") sql.appendUpdate(pending);
      const { room } = makeRoom(sql);
      const append = (bytes: Uint8Array) => room.fetch(authedRequest("/append", "user-a", { method: "POST", body: bytes }));
      const response = await append(source.export({ mode: "shallow-snapshot", frontiers: floor }));
      if (serverState === "retained pending") {
        expect(response.status).toBe(400);
        const retained = [...sql.exec("SELECT bytes FROM updates ORDER BY seq")];
        expect(Buffer.compare(new Uint8Array(retained[0].bytes as ArrayBuffer), pending)).toBe(0);
        expect((await append(source.export({ mode: "snapshot" }))).status).toBe(200);
      } else expect(response.status).toBe(200);
      const admitted = source.oplogVersion();
      try {
        metadata.set("afterBootstrap", true); source.commit();
        expect((await append(source.export({ mode: "update", from: admitted }))).status).toBe(200);
      } finally { admitted.free(); }
      const recovered = await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc();
      mirror.import(recovered.export({ mode: "snapshot" }));
      expect(mirror.toJSON()).toEqual(source.toJSON());
      const expected = source.oplogVersion();
      const actual = recovered.oplogVersion();
      try { expect(actual.compare(expected)).toBe(0); }
      finally { actual.free(); expected.free(); }
    } finally { mirror.free(); metadata.free(); source.free(); }
  });

  it("heals a 343-row workspace after repeated replay deaths and merges stale concurrent writers losslessly", async () => {
    const source = new LoroDoc();
    const first = new LoroDoc();
    const second = new LoroDoc();
    const mirror = new LoroDoc();
    const metadata = source.getMap("metadata");
    const sessions = source.getMap("sessions");
    const commands = source.getMap("commands");
    const history = source.getList("history");
    try {
      metadata.set("payload", oversizedPayload());
      commands.set("removed", { id: "removed", status: "accepted" });
      source.commit();
      const baseline = source.export({ mode: "snapshot" });
      first.import(baseline); second.import(baseline);
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.meta.set("replayAttempts", "5");
      sql.putBlob("snapshot", baseline);
      const updates: Uint8Array[] = [];
      for (let i = 0; i < 343; i++) {
        const before = source.oplogVersion();
        try {
          metadata.set("payload", "current");
          commands.delete("removed");
          sessions.set(`session-${i % 21}`, { status: i % 2 ? "working" : "idle", updatedAt: Date.now() + i });
          history.push({ id: `event-${i}`, value: i });
          source.commit();
          updates.push(source.export({ mode: "update", from: before }));
        } finally { before.free(); }
      }
      // Preserved legacy writes can be causally reordered, not just a tidy log.
      for (const update of updates.reverse()) sql.appendUpdate(update);
      const { room } = makeRoom(sql);
      const internals = room as unknown as SessionRoomInternals;
      expect((await internals.ensureDoc()).toJSON()).toEqual(source.toJSON());
      expect(sql.meta.get("replayAttempts")).toBe("0");
      expect(sql.meta.get("lastReplayBatches")).toBe("1");
      expect(sql.updateCount()).toBe(0);
      for (const [client, key] of [[first, "offline-a"], [second, "offline-b"]] as const) {
        const map = client.getMap("metadata");
        try { map.set(key, "preserved"); client.commit(); }
        finally { map.free(); }
        const snapshot = client.export({ mode: "snapshot" });
        expect((await room.fetch(authedRequest("/append", "user-a", { method: "POST", body: snapshot }))).status).toBe(200);
        source.import(snapshot);
      }
      const restarted = makeRoom(sql).room as unknown as SessionRoomInternals;
      const recovered = await restarted.ensureDoc();
      mirror.import(recovered.export({ mode: "snapshot" }));
      expect(mirror.toJSON()).toEqual(source.toJSON());
      expect(sql.meta.get("lastReplayRows")).toBe("0");
      const expectedVersion = source.oplogVersion();
      const restoredVersion = recovered.oplogVersion();
      try { expect(restoredVersion.compare(expectedVersion)).toBe(0); }
      finally { restoredVersion.free(); expectedVersion.free(); }
      first.import(mirror.export({ mode: "snapshot" }));
      second.import(mirror.export({ mode: "snapshot" }));
      expect(first.toJSON()).toEqual(source.toJSON());
      expect(second.toJSON()).toEqual(source.toJSON());
    } finally {
      history.free(); commands.free(); sessions.free(); metadata.free();
      mirror.free(); second.free(); first.free(); source.free();
    }
  });

  it("keeps burst-accepted workspace history bounded at restart without waiting for a flush timer", async () => {
    const sql = new MemorySql();
    sql.meta.set("roomKind", "workspace");
    sql.meta.set("owner", PROJECT_SCOPE);
    const { room } = makeRoom(sql);
    const source = new LoroDoc();
    const history = source.getList("history");
    const mirror = new LoroDoc();
    try {
      for (let i = 0; i < 70; i++) {
        const before = source.oplogVersion();
        try {
          history.push({ id: `accepted-${i}`, value: i }); source.commit();
          const response = await room.fetch(authedRequest("/append", "user-a", {
            method: "POST", body: source.export({ mode: "update", from: before })
          }));
          expect(response.status).toBe(200);
          // Performance boundary, not retention: all causal history still exists.
          expect(sql.updateCount()).toBeLessThan(32);
        } finally { before.free(); }
      }
      const recovered = await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc();
      mirror.import(recovered.export({ mode: "snapshot" }));
      expect(mirror.toJSON()).toEqual(source.toJSON());
      const expected = source.oplogVersion();
      const actual = recovered.oplogVersion();
      try { expect(actual.compare(expected)).toBe(0); }
      finally { actual.free(); expected.free(); }
    } finally { mirror.free(); history.free(); source.free(); }
  });

  it("retains accepted baseline and deltas when a snapshot transaction fails after log deletion", async () => {
    const source = new LoroDoc();
    const metadata = source.getMap("metadata");
    const sql = new MemorySql();
    sql.meta.set("roomKind", "workspace");
    sql.meta.set("owner", PROJECT_SCOPE);
    const { room } = makeRoom(sql);
    const append = (bytes: Uint8Array) => room.fetch(authedRequest("/append", "user-a", { method: "POST", body: bytes }));
    try {
      metadata.set("baseline", true); source.commit();
      expect((await append(source.export({ mode: "snapshot" }))).status).toBe(200);
      const before = source.oplogVersion();
      try {
        metadata.set("acceptedDelta", true); source.commit();
        expect((await append(source.export({ mode: "update", from: before }))).status).toBe(200);
      } finally { before.free(); }
      const expected = source.toJSON();
      const acceptedRows = sql.updateCount();
      metadata.set("unaccepted", true); source.commit();
      const originalExec = sql.exec.bind(sql);
      const failing = vi.spyOn(sql, "exec").mockImplementation((query, ...bindings) => {
        if (query.startsWith("INSERT INTO meta") && bindings[0] === "updateBytes" && bindings[1] === "0") {
          throw new Error("durable storage write failed");
        }
        return originalExec(query, ...bindings);
      });
      try { expect((await append(source.export({ mode: "snapshot" }))).status).toBe(400); }
      finally { failing.mockRestore(); }
      expect(sql.updateCount()).toBe(acceptedRows);
      expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(expected);
      expect((await (makeRoom(sql).room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(expected);
    } finally { metadata.free(); source.free(); }
  });

  it("lets different authenticated users join within one project scope", async () => {
    const { room, sql } = makeRoom();

    await join(room, "user-a", "shared-chat");
    await join(room, "user-b", "shared-chat");
    expect(sql.meta.get("owner")).toBe(PROJECT_SCOPE);
  });

  it("routes peers only to an eligible owner host, not project members or granted readers", async () => {
    const NativeResponse = Response;
    vi.stubGlobal("Response", class extends NativeResponse {
      constructor(body?: BodyInit | null, init?: ResponseInit) {
        super(body, init?.status === 101 ? { ...init, status: 200 } : init);
        if (init?.status === 101) Object.defineProperty(this, "status", { value: 101 });
      }
    });
    vi.stubGlobal("WebSocketPair", class { 0 = new CapturingSocket(); 1 = new CapturingSocket(); });
    const sessionId = "22222222-2222-4222-8222-222222222222";
    const target = makeRoom();
    await target.room.fetch(authedRequest(`/ws?chatId=${sessionId}`, "user-a"));
    await target.room.fetch(authedRequest(`/ws?chatId=${sessionId}&device=member-engine`, "user-b"));
    const grant = {
      userId: "user-a", email: "user-a@example.com", grantId: "1".repeat(32),
      projectId: PROJECT_SCOPE, deploymentId: "deployment-a", sessionId,
      sandboxId: "sandbox-a", targetDeviceId: "comet-scaffold-sandbox-a-e1",
      lifecycleEpoch: 1, capabilities: [...CAPABILITIES],
      grantedAt: Date.now() - 1, expiresAt: Date.now() + 600_000, revokedAt: null
    };
    await target.room.fetch(authedRequest(`/ws?chatId=${sessionId}&device=reader-engine`, "user-a", {
      headers: { [AUTH_GRANT_HEADER]: JSON.stringify({
        ...grant, subject: grant.userId, scope: {
          projectId: PROJECT_SCOPE, deploymentId: grant.deploymentId, sessionId, lifecycleEpoch: 1
        }
      }) }
    }));
    expect(target.sql.meta.get("hostDeviceId")).toBeUndefined();
    target.sql.meta.set("hostDeviceId", "historically-poisoned-engine");
    const routed: string[] = [];
    const env = {
      SCAFFOLD_PROJECT_SCOPE: PROJECT_SCOPE,
      AUTH_GRANTS: { idFromName: (id: string) => id, get: () => ({ fetch: async () => Response.json(grant) }) },
      SESSION_ROOMS: { idFromName: (id: string) => id, get: () => ({ fetch: (request: Request) => target.room.fetch(request) }) },
      DEVICE_ROOMS: { idFromName: (id: string) => id, get: (id: string) => ({ fetch: async () => {
        routed.push(id); return new Response(null, { status: 204 });
      } }) }
    } as unknown as Env;
    const request = () => worker.fetch(new Request(`https://edge.test/peer/${sessionId}/ws?syncProtocol=${DURABLE_SYNC_PROTOCOL}`, {
      headers: { authorization: `Bearer cs1.${grant.grantId}.${"a".repeat(64)}`, upgrade: "websocket" }
    }), env);
    expect((await request()).status).toBe(404);
    expect(routed).toEqual([]);
    await target.room.fetch(authedRequest(`/ws?chatId=${sessionId}&device=owner-engine`, "user-a"));
    expect((await request()).status).toBe(204);
    expect(routed).toEqual([`d3/${PROJECT_SCOPE}/owner-engine`]);
    target.sql.meta.delete("hostDeviceId");
    expect((await request()).status).toBe(204);
    expect(routed).toEqual([`d3/${PROJECT_SCOPE}/owner-engine`, `d3/${PROJECT_SCOPE}/owner-engine`]);
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
    const request = (path: string, method = "GET") => worker.fetch(new Request(`https://edge.test${path}${path.includes("?") ? "&" : "?"}syncProtocol=${DURABLE_SYNC_PROTOCOL}`, {
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

  it("isolates disaster backups for rooms sharing a session UUID and preserves legacy objects", async () => {
    const objects = new Map<string, Uint8Array>([["backup/shared-chat/latest.loro", new Uint8Array([1, 2, 3])]]);
    for (const roomId of ["legacy-room", "deployment-room"]) {
      const source = new LoroDoc();
      try {
        source.getMap("metadata").set("room", roomId);
        const sql = new MemorySql();
        sql.meta.set("roomKind", "workspace");
        sql.meta.set("chatId", "shared-chat");
        sql.meta.set("backupDirty", "1");
        sql.putBlob("snapshot", source.export({ mode: "snapshot" }));
        const { room } = makeRoom(sql, undefined, undefined, async (key, bytes) => { objects.set(key, bytes); }, undefined, roomId);
        await room.alarm();
      } finally { source.free(); }
    }
    for (const roomId of ["legacy-room", "deployment-room"]) {
      const recovered = new LoroDoc();
      try {
        recovered.import(objects.get(`backup/rooms/${roomId}/latest.loro`)!);
        expect(recovered.toJSON()).toEqual({ metadata: { room: roomId } });
      } finally { recovered.free(); }
    }
    expect(objects.get("backup/shared-chat/latest.loro")).toEqual(new Uint8Array([1, 2, 3]));
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
        body: catchup.export({ mode: "shallow-snapshot", frontiers: source.frontiers() })
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

  it("rejects metadata-decodable corrupt reset seeds without replacing baseline or journal", async () => {
    const original = new LoroDoc();
    const replacement = new LoroDoc();
    const restored = new LoroDoc();
    let baselineVersion: VersionVector | undefined;
    try {
      original.getMap("metadata").set("baseline", true);
      original.commit();
      const baseline = original.export({ mode: "snapshot" });
      baselineVersion = original.version();
      original.getMap("metadata").set("journal", "accepted");
      original.commit();
      const delta = original.export({ mode: "update", from: baselineVersion });
      const sql = new MemorySql();
      sql.meta.set("roomKind", "workspace");
      sql.meta.set("owner", PROJECT_SCOPE);
      sql.putBlob("snapshot", baseline);
      sql.appendUpdate(delta);
      replacement.getMap("metadata").set("replacement", true);
      const corrupt = replacement.export({ mode: "snapshot" });
      // Pinned Loro stores the snapshot checksum at bytes 16–19.
      corrupt[16] ^= 1;
      const metadata = decodeImportBlobMeta(corrupt, false);
      try { expect(metadata.mode).toBe("snapshot"); }
      finally { metadata.partialStartVersionVector.free(); metadata.partialEndVersionVector.free(); }
      const response = await makeRoom(sql).room.fetch(authedRequest("/reset-log", "user-a", {
        method: "POST", headers: { [ROOM_KIND_HEADER]: "workspace" }, body: corrupt
      }));
      expect(response.status).toBe(400);
      expect(sql.updateCount()).toBe(1);
      expect(new Uint8Array([...sql.exec("SELECT bytes FROM blobs WHERE name = ?", "snapshot")][0].bytes as ArrayBuffer)).toEqual(baseline);
      expect(new Uint8Array([...sql.exec("SELECT bytes FROM updates ORDER BY seq")][0].bytes as ArrayBuffer)).toEqual(delta);
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      restored.import(new Uint8Array(await cold.arrayBuffer()));
      expect(restored.toJSON()).toEqual({ metadata: { baseline: true, journal: "accepted" } });
    } finally { baselineVersion?.free(); restored.free(); replacement.free(); original.free(); }
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
      const state: JoinState = { userId: "user-a", projectScope: PROJECT_SCOPE, capabilities: CAPABILITIES, durableSync: true, rooms: [] };
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
      const healed = source.oplogVersion();
      try {
        map.set("newAccepted", "durable after legacy recovery"); source.commit();
        socket.sent.length = 0;
        await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode({
          type: MessageType.DocUpdate, crdt: CrdtType.Loro, roomId: "bootstrap-chat",
          batchId: "0x0000000000000003", updates: [source.export({ mode: "update", from: healed })]
        })).buffer);
        expect(socket.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      } finally { healed.free(); }
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
      durableSync: true,
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
        durableSync: true,
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

  it("expires room-wide fragment reservations so another authorized peer can complete recovery", async () => {
    const { room } = makeRoom();
    const first = await join(room, "user-a", "fragment-chat");
    const second = await join(room, "user-a", "fragment-chat");
    first.sent.length = 0; second.sent.length = 0;
    const envelope = { crdt: CrdtType.Loro, roomId: "fragment-chat" };
    const send = async (socket: CapturingSocket, message: ProtocolMessage) => {
      await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
    };
    await send(first, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
      batchId: "0x0000000000000001", fragmentCount: 1, totalSizeBytes: 64 * 1024 * 1024 });
    await send(second, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
      batchId: "0x0000000000000002", fragmentCount: 1, totalSizeBytes: 1 });
    expect(second.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.PayloadTooLarge })]);
    vi.advanceTimersByTime(30_001);
    await send(first, { type: MessageType.DocUpdateFragment, ...envelope,
      batchId: "0x0000000000000001", index: 0, fragment: new Uint8Array([1]) });
    expect(first.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.FragmentTimeout })]);
    const source = new LoroDoc();
    const metadata = source.getMap("metadata");
    try {
      metadata.set("recovered", true); source.commit();
      const snapshot = source.export({ mode: "snapshot" });
      await send(second, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
        batchId: "0x0000000000000003", fragmentCount: 1, totalSizeBytes: snapshot.byteLength });
      await send(second, { type: MessageType.DocUpdateFragment, ...envelope,
        batchId: "0x0000000000000003", index: 0, fragment: snapshot });
      expect(second.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(source.toJSON());
    } finally { metadata.free(); source.free(); }
  });

  it("keeps assembled payloads reserved until durable ACK rather than allowing concurrent unbounded recovery", async () => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    const { room } = makeRoom(new MemorySql(), async () => {
      if (!pause) return;
      pause = false; entered.resolve(); await resume.promise;
    });
    const first = await join(room, "user-a", "fragment-chat");
    const second = await join(room, "user-a", "fragment-chat");
    first.sent.length = 0; second.sent.length = 0;
    const source = new LoroDoc();
    const metadata = source.getMap("metadata");
    const envelope = { crdt: CrdtType.Loro, roomId: "fragment-chat" };
    const send = async (socket: CapturingSocket, message: ProtocolMessage) => {
      await room.webSocketMessage(socket as unknown as WebSocket, Uint8Array.from(encode(message)).buffer);
    };
    let finishing: Promise<void> | undefined;
    try {
      metadata.set("accepted", true); source.commit();
      const snapshot = source.export({ mode: "snapshot" });
      await send(first, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
        batchId: "0x0000000000000001", fragmentCount: 1, totalSizeBytes: snapshot.byteLength });
      pause = true;
      finishing = send(first, { type: MessageType.DocUpdateFragment, ...envelope,
        batchId: "0x0000000000000001", index: 0, fragment: snapshot });
      await entered.promise;
      await send(second, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
        batchId: "0x0000000000000002", fragmentCount: 1, totalSizeBytes: 64 * 1024 * 1024 });
      expect(second.sent.map((bytes) => decode(bytes))).toEqual([expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.PayloadTooLarge })]);
      resume.resolve(); await finishing;
      expect(first.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      // Completion releases the reservation, so a subsequent healthy retry fits.
      const next = source.oplogVersion();
      try {
        metadata.set("next", true); source.commit();
        const delta = source.export({ mode: "update", from: next });
        await send(second, { type: MessageType.DocUpdateFragmentHeader, ...envelope,
          batchId: "0x0000000000000003", fragmentCount: 1, totalSizeBytes: delta.byteLength });
        await send(second, { type: MessageType.DocUpdateFragment, ...envelope,
          batchId: "0x0000000000000003", index: 0, fragment: delta });
        expect(second.sent.map((bytes) => decode(bytes))).toContainEqual(expect.objectContaining({ type: MessageType.Ack, status: UpdateStatusCode.Ok }));
      } finally { next.free(); }
      expect((await (room as unknown as SessionRoomInternals).ensureDoc()).toJSON()).toEqual(source.toJSON());
    } finally { resume.resolve(); await finishing; metadata.free(); source.free(); }
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

  it("preserves writes arriving while a chunked workspace snapshot becomes durable", async () => {
    const entered = Promise.withResolvers<void>();
    const resume = Promise.withResolvers<void>();
    let pause = false;
    const sql = new MemorySql();
    sql.meta.set("roomKind", "workspace");
    sql.meta.set("owner", PROJECT_SCOPE);
    const { room } = makeRoom(sql, async () => {
      if (!pause) return;
      pause = false;
      entered.resolve();
      await resume.promise;
    });
    const source = new LoroDoc();
    const metadata = source.getMap("metadata");
    const mirror = new LoroDoc();
    const append = (bytes: Uint8Array) => room.fetch(
      authedRequest("/append", "user-a", { method: "POST", body: bytes })
    );
    let replacing: Promise<Response> | undefined;
    try {
      metadata.set("payload", oversizedPayload());
      source.commit();
      expect((await append(source.export({ mode: "snapshot" }))).status).toBe(200);
      metadata.set("payload", "latest");
      source.commit();
      pause = true;
      replacing = append(source.export({ mode: "snapshot" }));
      await entered.promise;
      const before = source.oplogVersion();
      let delta: Uint8Array;
      try {
        metadata.set("duringPersistence", true);
        source.commit();
        delta = source.export({ mode: "update", from: before });
      } finally { before.free(); }
      expect((await append(delta)).status).toBe(200);
      resume.resolve();
      expect((await replacing).status).toBe(200);
      // No stats, close handler, alarm or grace-period flush precedes restart.
      const cold = await makeRoom(sql).room.fetch(authedRequest("/snapshot", "user-a"));
      mirror.import(new Uint8Array(await cold.arrayBuffer()));
      expect(mirror.toJSON()).toEqual({ metadata: { payload: "latest", duringPersistence: true } });
      const version = source.oplogVersion();
      const recoveredVersion = mirror.oplogVersion();
      try { expect(recoveredVersion.compare(version)).toBe(0); }
      finally { recoveredVersion.free(); version.free(); }
    } finally {
      resume.resolve();
      await replacing;
      mirror.free(); metadata.free(); source.free();
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

  it.each(["workspace", "session"] as const)("preserves rejected %s history through replay failures and cold restart", async (roomKind) => {
    const corruptSnapshot = new Uint8Array(
      readFileSync(
        fileURLToPath(new NodeUrl("./fixtures/corrupt-loro-snapshot.bin", import.meta.url))
      )
    );

    for (const source of ["snapshot", "update"] as const) {
      const sql = new MemorySql();
      sql.meta.set("roomKind", roomKind);
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
