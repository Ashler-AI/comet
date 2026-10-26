import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LoroDoc } from "loro-crdt";
import { authenticateResult, type Verified } from "./auth";
import { ChatRoom } from "./chat-room";
import { encodeFrame, FRAME } from "./chat-frames";
import { rowsAfter } from "./chat-log";
import type { Env } from "./env";
import type * as AuthModule from "./auth";

vi.mock("./auth", async importOriginal => ({
  ...await importOriginal<typeof AuthModule>(),
  authenticateResult: vi.fn(),
}));

const identity: Verified = {
  userId: "owner", email: "owner@example.test", projectScope: "test", orgId: "test",
  deploymentId: "staging", credential: "scaffold", capabilities: ["session.read", "session.sync.write"],
};
const databases: DatabaseSync[] = [];
afterEach(() => {
  databases.splice(0).forEach(db => db.close());
  vi.unstubAllGlobals();
  vi.resetAllMocks();
});
function fixture() {
  vi.stubGlobal("WebSocketRequestResponsePair", class {});
  const db = new DatabaseSync(":memory:");
  databases.push(db);
  const sql = {
    exec(query: string, ...args: unknown[]) {
      return db.prepare(query).all(...args.map(value => value instanceof ArrayBuffer ? new Uint8Array(value) : value) as SQLInputValue[]);
    },
  } as unknown as SqlStorage;
  // Real SQLite and production appendRow; only the DO host/alarm surface is adapted.
  const ctx = {
    storage: {
      sql,
      transaction: <T>(work: () => Promise<T>) => work(),
      getAlarm: async () => null,
      setAlarm: async () => {},
    },
    setWebSocketAutoResponse() {},
    getWebSockets: () => [],
    waitUntil: (pending: Promise<unknown>) => { void pending; },
  } as unknown as DurableObjectState;
  const room = new ChatRoom(ctx, {} as Env);
  function socket(device: string) {
    const attachment = {
      ready: true, device, userId: identity.userId, chatId: "owned-chat",
      authority: {
        token: device, userId: identity.userId, projectId: identity.projectScope,
        deploymentId: identity.deploymentId, credential: identity.credential,
        capabilities: identity.capabilities, writable: true,
      },
    };
    const ws = {
      readyState: 1,
      deserializeAttachment: () => attachment,
      close: vi.fn(() => { ws.readyState = 3; }),
      send: vi.fn(),
    };
    return ws as unknown as WebSocket;
  }
  return { room, sql, socket };
}
function push(batchId: string, bytes: Uint8Array) {
  return encodeFrame(FRAME.push, { batchId }, bytes).buffer as ArrayBuffer;
}
function dependentUpdates() {
  const source = new LoroDoc();
  source.setPeerId("7");
  source.getText("body").insert(0, "parent");
  source.commit();
  const parent = source.export({ mode: "update" });
  const version = source.oplogVersion();
  source.getText("body").insert(6, " child");
  source.commit();
  const child = source.export({ mode: "update", from: version });
  version.free();
  source.free();
  return { parent, child };
}

describe("chat2 receive order", () => {
  it("persists causal PUSH order across delayed authorization without blocking another socket", async () => {
    const { room, sql, socket } = fixture();
    const first = socket("first"), other = socket("other");
    const held = Promise.withResolvers<void>();
    let calls = 0;
    vi.mocked(authenticateResult).mockImplementation(async () => {
      if (++calls === 1) await held.promise;
      return { status: "authenticated", identity };
    });
    const { parent, child } = dependentUpdates();
    const a = room.webSocketMessage(first, push("parent", parent));
    const b = room.webSocketMessage(first, push("child", child));
    try {
      // This completion also lets the ungated child run on the broken handler.
      await room.webSocketMessage(other, push("independent", parent));
      expect([...rowsAfter(sql, 0)].map(row => row.batchId)).toEqual(["independent"]);
    } finally {
      held.resolve();
      await Promise.all([a, b]);
    }
    const rows = [...rowsAfter(sql, 0)].filter(row => row.device === "first");
    expect(rows.map(row => row.batchId)).toEqual(["parent", "child"]);
    const receiver = new LoroDoc();
    try {
      for (const row of rows) expect(receiver.import(row.bytes).pending).toBeNull();
      expect(receiver.getText("body").toString()).toBe("parent child");
    } finally { receiver.free(); }
  });

  it.each(["close", "error", "revoked"])("discards queued writes on %s while authorization is pending", async reason => {
    const { room, sql, socket } = fixture();
    const ws = socket("first"), held = Promise.withResolvers<void>();
    let calls = 0;
    vi.mocked(authenticateResult).mockImplementation(async () => {
      if (++calls === 1) await held.promise;
      return reason === "revoked" ? { status: "invalid" } : { status: "authenticated", identity };
    });
    const { parent, child } = dependentUpdates();
    const a = room.webSocketMessage(ws, push("parent", parent));
    const b = room.webSocketMessage(ws, push("child", child));
    if (reason === "close") await room.webSocketClose(ws);
    if (reason === "error") await room.webSocketError(ws);
    held.resolve();
    await Promise.all([a, b]);
    expect([...rowsAfter(sql, 0)]).toEqual([]);
    expect(calls).toBe(1);
    // A late event on the same closed socket must not start another drain.
    await room.webSocketMessage(ws, push("late", child));
    expect(calls).toBe(1);
  });

  it.each(["count", "bytes"])("bounds per-socket queued %s including the active authorization and recovers after overflow", async dimension => {
    const { room, sql, socket } = fixture();
    const ws = socket("first"), held = Promise.withResolvers<void>();
    vi.mocked(authenticateResult).mockImplementation(async () => {
      await held.promise;
      return { status: "authenticated", identity };
    });
    const frame = dimension === "count" ? push("batch", new Uint8Array([1]))
      : push("batch", new Uint8Array(1024 * 1024 - push("batch", new Uint8Array()).byteLength));
    const limit = dimension === "count" ? 300 : 8;
    const tasks = Array.from({ length: limit }, () => room.webSocketMessage(ws, frame));
    try {
      expect(ws.close).not.toHaveBeenCalled();
      await room.webSocketMessage(ws, dimension === "count" ? frame : push("extra", new Uint8Array([1])));
      expect(ws.close).toHaveBeenCalledWith(1013, "receive queue full");
    } finally {
      held.resolve();
      await Promise.all(tasks);
    }
    expect([...rowsAfter(sql, 0)]).toEqual([]);
    const { parent } = dependentUpdates();
    await room.webSocketMessage(socket("reconnected"), push("recovered", parent));
    expect([...rowsAfter(sql, 0)].map(row => row.batchId)).toEqual(["recovered"]);
  });

  it.each(["count", "bytes"])("bounds aggregate queued %s across sockets and releases it on close", async dimension => {
    const { room, sql, socket } = fixture();
    const held = Promise.withResolvers<void>();
    vi.mocked(authenticateResult).mockImplementation(async () => {
      await held.promise;
      return { status: "authenticated", identity };
    });
    const frame = dimension === "count" ? push("batch", new Uint8Array([1]))
      : push("batch", new Uint8Array(1024 * 1024 - push("batch", new Uint8Array()).byteLength));
    const limit = dimension === "count" ? 300 : 8;
    const sockets = Array.from({ length: 4 }, (_, index) => socket(`socket-${index}`));
    const tasks = sockets.flatMap(ws => Array.from({ length: limit }, () => room.webSocketMessage(ws, frame)));
    const extra = socket("extra");
    try {
      sockets.forEach(ws => expect(ws.close).not.toHaveBeenCalled());
      await room.webSocketMessage(extra, push("extra", new Uint8Array([1])));
      expect(extra.close).toHaveBeenCalledWith(1013, "receive queue full");
    } finally {
      for (const ws of sockets) await room.webSocketClose(ws);
      held.resolve();
      await Promise.all(tasks);
    }
    expect([...rowsAfter(sql, 0)]).toEqual([]);
    const { parent } = dependentUpdates();
    await room.webSocketMessage(socket("reconnected"), push("recovered", parent));
    expect([...rowsAfter(sql, 0)].map(row => row.batchId)).toEqual(["recovered"]);
  });

  it.each(["userId", "deploymentId", "lifecycleEpoch"] as const)("revalidates queued frames against changed %s", async field => {
    const { room, sql, socket } = fixture();
    const ws = socket("first");
    const valid = { status: "authenticated" as const, identity };
    vi.mocked(authenticateResult)
      .mockResolvedValueOnce(valid) // Parent inbound authorization.
      .mockResolvedValueOnce(valid) // Parent ACK outbound authorization.
      .mockResolvedValue({ status: "authenticated", identity: { ...identity, [field]: field === "lifecycleEpoch" ? 2 : "different" } });
    const { parent, child } = dependentUpdates();
    await Promise.all([
      room.webSocketMessage(ws, push("parent", parent)),
      room.webSocketMessage(ws, push("child", child)),
    ]);
    expect([...rowsAfter(sql, 0)].map(row => row.batchId)).toEqual(["parent"]);
    expect(ws.close).toHaveBeenCalledWith(4401, "authority expired or revoked");
    expect(authenticateResult).toHaveBeenCalledTimes(3);
  });

  it("keeps active frames charged after close until authorization settles", async () => {
    const { room, sql, socket } = fixture();
    const held = Promise.withResolvers<void>();
    vi.mocked(authenticateResult).mockImplementation(async () => {
      await held.promise;
      return { status: "authenticated", identity };
    });
    const frame = push("batch", new Uint8Array(1024 * 1024 - push("batch", new Uint8Array()).byteLength));
    const sockets = Array.from({ length: 32 }, (_, index) => socket(`socket-${index}`));
    const tasks = sockets.map(ws => room.webSocketMessage(ws, frame));
    try {
      for (const ws of sockets) await room.webSocketClose(ws);
      const extra = socket("extra");
      await room.webSocketMessage(extra, push("extra", new Uint8Array([1])));
      expect(extra.close).toHaveBeenCalledWith(1013, "receive queue full");
    } finally {
      held.resolve();
      await Promise.all(tasks);
    }
    expect([...rowsAfter(sql, 0)]).toEqual([]);
    const { parent } = dependentUpdates();
    await room.webSocketMessage(socket("reconnected"), push("recovered", parent));
    expect([...rowsAfter(sql, 0)].map(row => row.batchId)).toEqual(["recovered"]);
  });
});
