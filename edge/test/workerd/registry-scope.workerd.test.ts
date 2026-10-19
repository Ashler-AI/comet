import { env } from "cloudflare:test";
import { expect, it } from "vitest";
import { AUTHORITY_HEADER, requestAuthority, trustedRequest, type SocketAuthority } from "../../src/authority";
import { authenticateDeviceToken } from "../../src/auth-routes";
import { AUTH_USER_HEADER, type Env } from "../../src/env";
import type { Op, Row } from "../../src/registry-core";

const host: SocketAuthority = {
  credential: "device", token: "test-boundary", userId: "owner", projectId: "canary", deploymentId: "canary",
  sessionId: "one", sandboxId: "box", lifecycleEpoch: 2, deviceId: "comet-scaffold-box-e2",
  writable: true, capabilities: ["session.read", "session.sync.write"],
};
const scope = { projectId: "canary", deploymentId: "canary", sessionId: "one" };
const crew = { ownerSubject: "owner", sessionId: "one", scope,
  environment: { ownerPrincipal: "owner", scope, source: { kind: "scaffold", sandbox_id: "box", lifecycle_epoch: 2, lifecycle: "ready" } },
  agentRoute: { provider: "openai", model: "owner-selected", fallback: "disabled", routingMode: "automatic" },
};
const op = (kind: string, id: string, set: NonNullable<Op["set"]>, tick = 1): Op =>
  ({ kind, id, set, op: "upsert", hlc: `${String(tick).padStart(13, "0")}-000000-host` });
const headers = (authority?: SocketAuthority) => ({ [AUTH_USER_HEADER]: "owner", ...(authority ? { [AUTHORITY_HEADER]: JSON.stringify(authority) } : {}) });
const room = () => env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(crypto.randomUUID()));
async function push(stub: DurableObjectStub, ops: Op[], authority?: SocketAuthority) {
  return stub.fetch("https://registry/push", { method: "POST", headers: headers(authority), body: JSON.stringify({ batch: crypto.randomUUID(), ops }) });
}
async function state(stub: DurableObjectStub, authority?: SocketAuthority, query = "") {
  const response = await stub.fetch(`https://registry/rows${query}`, { headers: headers(authority) });
  expect(response.status).toBe(200);
  return response.json<{ seq: number; rows: Row[]; presence: Record<string, number> }>();
}

it("bootstraps device and space before chat without expanding the scoped view", async () => {
  const stub = room();
  expect((await push(stub, [op("devices", host.deviceId!, { id: host.deviceId!, name: "sandbox", platform: "linux", capabilities: ["native-omp"] })], host)).status).toBe(200);
  expect((await push(stub, [op("spaces", "space", { id: "space", deviceId: host.deviceId!, path: "/workspace/crew-handoff", createdAt: 1 })], host)).status).toBe(200);
  expect((await push(stub, [op("chats", "one", { id: "one", deviceId: host.deviceId!, spaceId: "space", crew: null, title: "Native", cwd: "/workspace/crew-handoff", harnessSessionId: "omp-123" })], host)).status).toBe(200);
  expect((await state(stub, host)).rows.map(row => row.id).sort()).toEqual([host.deviceId!, "one", "space"].sort());
  expect((await push(stub, [op("sessions", "one", { chatId: "one", deviceId: host.deviceId!, status: "working", runningSubagents: 1, updatedAt: 10 })], host)).status).toBe(200);
  expect((await state(stub)).rows.find(row => row.kind === "sessions")?.fields.status).toBe("working");
});

it("publishes scoped host completion to the owner without admitting malformed or foreign status batches", async () => {
  const stub = room();
  const working = { chatId: "one", deviceId: host.deviceId!, status: "working", lastCompletedTurn: null, startedAt: 10, updatedAt: 10, runningSubagents: 0 };
  expect((await push(stub, [op("chats", "one", { id: "one", deviceId: host.deviceId!, crew })])).status).toBe(200);
  expect((await push(stub, [op("sessions", "one", working)], host)).status).toBe(200);
  const before = await state(stub);
  const completed = { ...working, status: "idle", lastCompletedTurn: "turn-one", startedAt: null, updatedAt: 20 };
  expect((await push(stub, [
    op("chats", "one", { lastMessagePreview: "CREW_CANARY_9C_RUNTIME_OK", lastMessageAt: 20 }, 2),
    op("sessions", "one", completed, 2),
  ], host)).status).toBe(200);
  const update = await state(stub, undefined, `?since=${before.seq}`);
  expect(update.rows.find(row => row.kind === "sessions")?.fields).toMatchObject({ status: "idle", lastCompletedTurn: "turn-one" });
  expect(update.rows.find(row => row.kind === "chats")?.fields.lastMessagePreview).toBe("CREW_CANARY_9C_RUNTIME_OK");
  const retained = await state(stub);
  for (const bad of [
    op("sessions", "one", { ...completed, lastCompletedTurn: 20 }, 3),
    op("sessions", "two", { ...completed, chatId: "two" }, 3),
    op("sessions", "one", { ...completed, deviceId: "desktop" }, 3),
  ]) {
    expect((await push(stub, [op("chats", "one", { title: "must not commit" }, 3), bad], host)).status).toBe(400);
    expect(await state(stub)).toEqual(retained);
  }
});

it("retains canonical metadata and filters initial/delta/repair and presence for two sessions", async () => {
  const stub = room();
  const config = { harness: "omp", model: "owner-selected", reasoning: "high", sandbox: "workspace-write", modelOptions: {} };
  expect((await push(stub, [
    op("chats", "one", { id: "one", deviceId: host.deviceId!, crew, config }),
    op("chats", "two", { id: "two", deviceId: "desktop", title: "private" }),
    op("devices", "desktop", { id: "desktop", name: "private" }),
    op("spaces", "foreign", { id: "foreign", deviceId: "desktop", path: "/Users/private" }),
    op("preferences", "owner", { secret: "private" }),
  ])).status).toBe(200);
  await state(stub, undefined, "?device=desktop&beat=1");
  const initial = await state(stub, host, "?beat=1");
  expect(initial.rows.map(row => row.id)).toEqual(["one"]);
  expect(Object.keys(initial.presence)).toEqual([host.deviceId]);
  expect((await push(stub, [op("chats", "one", { id: "one", deviceId: host.deviceId!, crew: { ownerSubject: "owner", sessionId: "one", scope }, config: null, title: "Host title", harnessSessionId: "native-id", harnessSessionCwd: "/workspace/crew-handoff" }, 2)], host)).status).toBe(200);
  for (const query of ["", `?since=${initial.seq}`, "?since=999999"]) {
    const own = await state(stub, host, query);
    expect(own.rows.map(row => row.id)).toEqual(["one"]);
    expect(own.rows[0].fields).toMatchObject({ crew, config, title: "Host title", harnessSessionId: "native-id" });
  }
  const owner = await state(stub);
  expect(owner.rows).toHaveLength(5);
  expect(owner.rows.find(row => row.id === "one")?.fields.title).toBe("Host title");
  const changedConfig = { ...config, model: "host-selected" };
  expect((await push(stub, [op("chats", "one", { config: changedConfig }, 3)], host)).status).toBe(200);
  expect((await state(stub)).rows.find(row => row.id === "one")?.fields.config).toEqual(changedConfig);
});

it("rejects other sessions/devices, raw columns, deletes, admin and stale epochs", async () => {
  const stub = room();
  await push(stub, [op("chats", "one", { id: "one", deviceId: host.deviceId!, crew }), op("spaces", "foreign", { id: "foreign", deviceId: "desktop", path: "/Users/private" })]);
  for (const bad of [
    op("chats", "two", { id: "two", deviceId: host.deviceId! }),
    op("devices", "desktop", { id: "desktop" }),
    op("spaces", "foreign", { id: "foreign", deviceId: host.deviceId!, path: "/workspace/repo" }),
    op("spaces", "escape", { id: "escape", deviceId: host.deviceId!, path: "/workspace/../etc" }),
    op("spaces", "outside", { id: "outside", deviceId: host.deviceId!, path: "/Users/private" }),
    op("chats", "one", { ownerSubject: "attacker" }),
    op("chats", "one", { crew: { ...crew, ownerSubject: "attacker" } }),
    op("sessions", "one", { chatId: "one", deviceId: "desktop" }),
    op("preferences", "owner", { theme: "bad" }),
    { kind: "chats", id: "one", op: "delete", hlc: "9999999999999-000000-host" } as Op,
  ]) expect((await push(stub, [bad], host)).status).toBe(400);
  for (const [path, method] of [["stats", "GET"], ["reset", "POST"], ["push-target", "POST"]]) {
    expect((await stub.fetch(`https://registry/${path}`, { method, headers: headers(host) })).status).toBe(403);
  }
  expect((await stub.fetch("https://registry/rows?device=desktop&beat=1", { headers: headers(host) })).status).toBe(403);
  const newer = { ...host, deviceId: "comet-scaffold-box-e3", lifecycleEpoch: 3 };
  const nextCrew = { ...crew, environment: { ...crew.environment, source: { ...crew.environment.source, lifecycle_epoch: 3 } } };
  await push(stub, [op("chats", "one", { deviceId: newer.deviceId, crew: nextCrew }, 10)]);
  expect((await stub.fetch("https://registry/rows", { headers: headers(host) })).status).toBe(403);
  expect((await push(stub, [op("chats", "one", { title: "stale" }, 999)], host)).status).toBe(403);
  expect((await push(stub, [op("chats", "one", { title: "new host" }, 11)], newer)).status).toBe(200);
  expect((await state(stub)).rows.find(row => row.id === "one")?.fields.title).toBe("new host");
});

async function issuedHost(): Promise<SocketAuthority> {
  const grantId = crypto.randomUUID().replaceAll("-", "");
  const secret = "a".repeat(64);
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(secret));
  const grantHash = btoa(String.fromCharCode(...new Uint8Array(digest))).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
  const grant = env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(grantId));
  expect((await grant.fetch("https://grant/issue", { method: "POST", body: JSON.stringify({
    grantId, ownerUserId: host.userId, email: "owner@example.com", projectId: host.projectId,
    deploymentId: host.deploymentId, sandboxId: host.sandboxId, targetDeviceId: host.deviceId,
    sessionId: host.sessionId, lifecycleEpoch: host.lifecycleEpoch, capabilities: host.capabilities,
    grantHash, issuedAt: Date.now(), grantExpiresAt: Date.now() + 60_000,
  }) })).status).toBe(200);
  const response = await grant.fetch("https://grant/exchange", { method: "POST", body: JSON.stringify({ secret }) });
  expect(response.status).toBe(200);
  const { accessToken } = await response.json<{ accessToken: string }>();
  const identity = await authenticateDeviceToken(env as unknown as Env, accessToken);
  expect(identity).toBeDefined();
  return requestAuthority(trustedRequest(new Request("https://registry/", { headers: { authorization: `Bearer ${accessToken}` } }), identity!))!;
}

function nextFrame(ws: WebSocket, kind: string): Promise<Record<string, unknown>> {
  const { promise, resolve } = Promise.withResolvers<Record<string, unknown>>();
  const onMessage = (event: MessageEvent) => {
    const frame = JSON.parse(event.data as string) as Record<string, unknown>;
    if (frame.t !== kind) return;
    ws.removeEventListener("message", onMessage);
    resolve(frame);
  };
  ws.addEventListener("message", onMessage);
  return promise;
}

function nextClose(ws: WebSocket): Promise<CloseEvent> {
  const { promise, resolve } = Promise.withResolvers<CloseEvent>();
  ws.addEventListener("close", resolve, { once: true });
  return promise;
}

it("filters live sockets, rejects forged hello/presence, reconnects and revokes real grants", async () => {
  const stub = room();
  const authority = await issuedHost();
  await push(stub, [op("chats", "one", { id: "one", deviceId: host.deviceId!, crew }), op("chats", "two", { id: "two", deviceId: "desktop" })]);
  const connect = async (bound?: SocketAuthority) => {
    const response = await stub.fetch("https://registry/ws", { headers: { ...headers(bound), upgrade: "websocket" } });
    expect(response.status).toBe(101);
    const socket = response.webSocket!;
    socket.accept();
    return socket;
  };
  const scoped = await connect(authority);
  const initial = nextFrame(scoped, "state");
  scoped.send(JSON.stringify({ t: "hello", device: authority.deviceId }));
  expect((await initial).rows).toEqual((await state(stub, authority)).rows);
  const observed: Record<string, unknown>[] = [];
  scoped.addEventListener("message", event => observed.push(JSON.parse(event.data as string)));
  const owner = await connect();
  const ownerInitial = nextFrame(owner, "state");
  owner.send(JSON.stringify({ t: "hello", device: "desktop" }));
  expect(((await ownerInitial).rows as Row[]).map(row => row.id).sort()).toEqual(["one", "two"]);
  const hidden = nextFrame(scoped, "rows");
  await push(stub, [op("chats", "two", { title: "private change" }, 2)]);
  expect((await hidden).rows).toEqual([]);
  const ownerUpdate = nextFrame(owner, "rows");
  const ack = nextFrame(scoped, "ack");
  scoped.send(JSON.stringify({ t: "push", batch: "native-update", ops: [op("chats", "one", { harnessSessionId: "native-live", title: "live" }, 3)] }));
  expect(((await ownerUpdate).rows as Row[])[0].fields.harnessSessionId).toBe("native-live");
  expect((await ack).batch).toBe("native-update");
  const ownerPresence = nextFrame(owner, "presence");
  scoped.send(JSON.stringify({ t: "presence", at: Number.MAX_SAFE_INTEGER }));
  expect(await ownerPresence).toMatchObject({ device: host.deviceId });
  await state(stub, undefined, "?device=desktop&beat=1");
  const probe = nextFrame(scoped, "probe-ok");
  scoped.send(JSON.stringify({ t: "probe" }));
  await probe;
  expect(observed.some(frame => frame.t === "presence" && frame.device === "desktop")).toBe(false);

  scoped.close();
  const reconnect = await connect(authority);
  const restored = nextFrame(reconnect, "state");
  reconnect.send(JSON.stringify({ t: "hello" }));
  expect(((await restored).rows as Row[])[0].fields.harnessSessionId).toBe("native-live");
  const forgedPresence = nextClose(reconnect);
  reconnect.send(JSON.stringify({ t: "presence", device: "desktop" }));
  expect((await forgedPresence).code).toBe(4403);
  const forged = await connect(authority);
  const closed = nextClose(forged);
  forged.send(JSON.stringify({ t: "hello", device: "desktop" }));
  expect((await closed).code).toBe(4403);

  const revoked = await connect(authority);
  const beforeRevoke = nextFrame(revoked, "state");
  revoked.send(JSON.stringify({ t: "hello" }));
  await beforeRevoke;
  const revokedClose = nextClose(revoked);
  const grant = env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(authority.grantId!));
  expect((await grant.fetch("https://grant/revoke", { method: "POST", body: JSON.stringify({ ownerUserId: host.userId }) })).status).toBe(200);
  await push(stub, [op("chats", "one", { title: "after revoke" }, 4)]);
  expect((await revokedClose).code).toBe(4401);
  owner.close();
});
