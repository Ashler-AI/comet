/// <reference types="node" />
/// <reference lib="es2024.promise" />
import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LoroDoc } from "loro-crdt";
import { authenticateResult, type Verified } from "./auth";
import { ChatRoom } from "../chat-room";
import { decodeFrame, encodeFrame, FRAME } from "../chat-frames";
import { getMeta, rowsAfter } from "../chat-log";
import { AUTH_USER_HEADER, type Env } from "../env";
import { AUTHORITY_HEADER, type SocketAuthority } from "./authority";
import type * as AuthModule from "./auth";

vi.mock("./auth", async importOriginal => ({
  ...await importOriginal<typeof AuthModule>(),
  authenticateResult: vi.fn(),
}));

const identity: Verified = {
  kind: "user", userId: "owner", email: "owner@example.test", projectScope: "test", orgId: "test",
  deploymentId: "staging", credential: "scaffold", capabilities: ["session.read", "session.sync.write"],
};
const databases: DatabaseSync[] = [];
afterEach(() => {
  databases.splice(0).forEach(db => db.close());
  vi.unstubAllGlobals();
  vi.resetAllMocks();
});
function fixture(env: Partial<Env> = {}) {
  vi.stubGlobal("WebSocketRequestResponsePair", class {});
  const pending: Promise<unknown>[] = [];
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
    waitUntil: (work: Promise<unknown>) => { pending.push(work); },
  } as unknown as DurableObjectState;
  const room = new ChatRoom(ctx, env as Env);
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
      serializeAttachment: (value: object) => { Object.assign(attachment, value); },
      close: vi.fn(() => { ws.readyState = 3; }),
      send: vi.fn(),
    };
    return ws as unknown as WebSocket;
  }
  /** Lets every deferred send (sendAuthorized) and wake settle. */
  const settle = async () => { while (pending.length) await Promise.allSettled(pending.splice(0)); };
  return { room, sql, socket, settle };
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

describe("Crew grafts on upstream's chat room", () => {
  const authority = (writable: boolean): SocketAuthority => ({
    token: "sc_rc_test", userId: identity.userId, projectId: identity.projectScope, deploymentId: identity.deploymentId,
    credential: "device", sessionId: "owned-chat", deviceId: "device-a", capabilities: ["session.read"], writable,
  });
  const request = (path: string, method: string, bound?: SocketAuthority, body?: Uint8Array, headers: Record<string, string> = {}) =>
    new Request(`https://chat.internal${path}`, {
      method, body, headers: { [AUTH_USER_HEADER]: identity.userId, ...(bound ? { [AUTHORITY_HEADER]: JSON.stringify(bound) } : {}), ...headers },
    });

  it.each([
    ["/checkpoint-part?offset=0", "POST"],
    ["/checkpoint?seqCovered=0&staged=4", "POST"],
    ["/checkpoint?seqCovered=0", "POST"],
    ["/rows?batchId=b&device=device-a&chatId=owned-chat", "POST"],
    ["/tail", "PUT"],
    ["/diff", "PUT"],
    ["/reset", "POST"],
  ])("refuses %s %s from a read-only authority, before it can claim the room", async (path, method) => {
    const { room, sql } = fixture();
    const response = await room.fetch(request(path, method, authority(false), new Uint8Array([1, 2, 3, 4])));
    expect(response.status).toBe(403);
    expect(await response.json()).toEqual({ error: "opaque_write_forbidden" });
    expect(getMeta(sql, "owner")).toBeUndefined();
  });

  it("lets a writable authority stage a checkpoint in parts and commit it; reads stay open", async () => {
    const { room } = fixture();
    const part = await room.fetch(request("/checkpoint-part?offset=0", "POST", authority(true), new Uint8Array([1, 2, 3, 4])));
    expect(await part.json()).toEqual({ ok: true, staged: 4 });
    const commit = await room.fetch(request("/checkpoint?seqCovered=0&staged=4", "POST", authority(true), undefined, { "x-chat2-frontier": "" }));
    expect(await commit.json()).toMatchObject({ ok: true, seqFloor: 0 });
    const stats = await room.fetch(request("/stats", "GET", authority(false)));
    expect(stats.status).toBe(200);
    expect(await stats.json()).toMatchObject({ checkpointSize: 4 });
  });

  it("answers ownership lookups without claiming, and binds an owner only once", async () => {
    const { room, sql } = fixture();
    const lookup = async (user: string) => (await room.fetch(new Request("https://chat.internal/directory-authority", {
      headers: { [AUTH_USER_HEADER]: user },
    }))).json();
    expect(await lookup(identity.userId)).toEqual({ ownsSession: false });
    expect(getMeta(sql, "owner")).toBeUndefined();
    const bind = (user: string) => room.fetch(new Request("https://chat.internal/bind-owner", { method: "POST", headers: { [AUTH_USER_HEADER]: user } }));
    expect((await bind(identity.userId)).status).toBe(200);
    expect((await bind("attacker")).status).toBe(403);
    expect(await lookup(identity.userId)).toEqual({ ownsSession: true });
    expect(await lookup("attacker")).toEqual({ ownsSession: false });
  });

  it.each([
    [{ CHAT_RELAY_LAYER: "1" }, true],
    [{ CHAT_RELAY_LAYER: "1", SCAFFOLD_PROJECT_SCOPE: "test" }, false],
  ] as const)("offers the relay layer under %j: %s", async (env, offered) => {
    const { room, socket, settle } = fixture(env);
    vi.mocked(authenticateResult).mockResolvedValue({ status: "authenticated", identity });
    const ws = socket("reader");
    await room.webSocketMessage(ws, encodeFrame(FRAME.hello, { device: "reader", relay: 1 }).buffer as ArrayBuffer);
    await settle();
    const sent = vi.mocked(ws.send).mock.calls.map(([data]) => decodeFrame(new Uint8Array(data as ArrayBuffer))!);
    const state = sent.find(frame => frame.type === FRAME.state);
    expect(state?.header.relay === 1).toBe(offered);
  });
});
