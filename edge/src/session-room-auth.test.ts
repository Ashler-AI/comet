import { readFileSync } from "node:fs";
import { fileURLToPath, URL as NodeUrl } from "node:url";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CrdtType, MessageType, UpdateStatusCode, decode, encode, type JoinRequest, type ProtocolMessage } from "loro-protocol";
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
}

interface SessionRoomInternals {
  eph?: unknown;
  ensureDoc(): Promise<LoroDoc>;
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

const makeRoom = (
  sql = new MemorySql()
): { room: SessionRoom; sql: MemorySql; sockets: WebSocket[] } => {
  const sockets: WebSocket[] = [];
  const storage = {
    sql: sql as unknown as SqlStorage,
    sync: async () => {},
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
  return { room: new SessionRoom(ctx, {} as Env), sql, sockets };
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

const join = async (room: SessionRoom, userId: string, roomId: string): Promise<CapturingSocket> => {
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
    joinRequest(roomId)
  );
  expect(state.rooms).toContain(CrdtType.Loro);
  expect(socket.sent.some((bytes) => decode(bytes).type === MessageType.JoinResponseOk)).toBe(true);
  return socket;
};

describe("SessionRoom chat authorization", () => {
  beforeEach(() => {
    vi.useFakeTimers();
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
