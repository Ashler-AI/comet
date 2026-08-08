import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CrdtType, MessageType, decode, type JoinRequest } from "loro-protocol";
import { AUTH_USER_HEADER, type Env } from "./env";
import { canonicalSessionId, SessionRoom } from "./session-room";

type SqlRow = Record<string, SqlStorageValue>;

const cursor = (rows: SqlRow[]): SqlStorageCursor<SqlRow> =>
  rows as unknown as SqlStorageCursor<SqlRow>;

class MemorySql {
  readonly meta = new Map<string, string>();
  private readonly blobs = new Map<string, Map<number, ArrayBuffer>>();
  private readonly updates: ArrayBuffer[] = [];

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

  close(): void {}
}

interface JoinState {
  /** Legacy attachment field used here to identify distinct authenticated
   * callers; authorization intentionally ignores it. */
  userId: string;
  rooms: string[];
  deviceId?: string;
}

interface SessionRoomInternals {
  handleJoin(ws: WebSocket, state: JoinState, message: JoinRequest): Promise<void>;
}

const makeRoom = (sql = new MemorySql()): { room: SessionRoom; sql: MemorySql } => {
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
  return { room: new SessionRoom(ctx, {} as Env), sql };
};

const authedRequest = (path: string, userId: string, init: RequestInit = {}): Request => {
  const headers = new Headers(init.headers);
  headers.set(AUTH_USER_HEADER, userId);
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
  const state: JoinState = { userId, rooms: [] };
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

  it("lets different authenticated users join a room with legacy owner metadata", async () => {
    const { room, sql } = makeRoom();
    sql.meta.set("owner", "legacy-owner");

    await join(room, "user-a", "shared-chat");
    await join(room, "user-b", "shared-chat");
  });

  it("lets different authenticated users mutate and read every routed chat surface", async () => {
    const { room, sql } = makeRoom();
    sql.meta.set("owner", "legacy-owner");

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

  it("keeps an unclaimed room live and empty without creating an owner claim", async () => {
    const { room, sql } = makeRoom();

    const emptyTail = await room.fetch(authedRequest("/tail", "user-a"));
    expect(emptyTail.status).toBe(200);
    await join(room, "user-a", "new-shared-chat");

    expect(sql.meta.get("chatId")).toBe("new-shared-chat");
    expect(sql.meta.has("owner")).toBe(false);
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
