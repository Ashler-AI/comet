import { env, SELF, runInDurableObject, evictDurableObject } from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import { AUTH_USER_HEADER, type Env } from "../../src/env";
import type { ScaffoldAuthenticationResult } from "../../src/auth";
import { handleAuthenticatedAuthRoute, handlePublicAuthRoute, scaffoldVerificationName, verifyScaffoldThroughCoordinator } from "../../src/auth-routes";
import { SCAFFOLD_JSON_BYTES, SCAFFOLD_TOKEN_BYTES } from "../../src/auth-envelope";

it("directory lookup never claims a chat and observes the actual chat2 owner", async () => {
  const room = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName(`chat2/${crypto.randomUUID()}`));
  const lookup = (user: string) => room.fetch(new Request("https://chat.internal/directory-authority", {
    headers: { [AUTH_USER_HEADER]: user },
  }));
  expect(await (await lookup("attacker")).json()).toEqual({ ownsSession: false });
  expect((await room.fetch(new Request("https://chat.internal/bind-owner", {
    method: "POST", headers: { [AUTH_USER_HEADER]: "owner" },
  }))).status).toBe(200);
  expect(await (await lookup("owner")).json()).toEqual({ ownsSession: true });
  expect(await (await lookup("attacker")).json()).toEqual({ ownsSession: false });
});

const runtimeEnv = env as unknown as Env;
const tokens: string[] = [];
const sockets: Array<{ socket: WebSocket; room: DurableObjectStub }> = [];
// Integration timing is intentional: workerd DO timers and the Node outbound
// service use real clocks; fake isolate timers cannot advance both authorities.
const pause = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));
const consumer = () => env.TEST_LOG.get(env.TEST_LOG.idFromName(crypto.randomUUID()));
const controlUrl = (token: string) => `https://authority-test.internal/?token=${encodeURIComponent(token)}`;
type UpstreamState = { status: number; retryAfter?: string; requests: Array<{ at: number; url: string }> };
const upstream = async (token: string, status: number, retryAfter?: string): Promise<UpstreamState> => {
  const response = await fetch(controlUrl(token), { method: "POST", body: JSON.stringify({ status, retryAfter }) });
  expect(response.status).toBe(200);
  return response.json<UpstreamState>();
};
const observed = async (token: string) => (await fetch(controlUrl(token))).json<UpstreamState>();
const newToken = async () => {
  const token = `sc_rc_${crypto.randomUUID()}`;
  tokens.push(token);
  await upstream(token, 200);
  return token;
};
const check = async (room: DurableObjectStub, token: string, overrides: Partial<Env> = {}) => {
  const response = await room.fetch("https://consumer.internal/authority/check", {
    method: "POST", headers: { authorization: `Bearer ${token}` }, body: JSON.stringify(overrides)
  });
  expect(response.status).toBe(200);
  return response.json<ScaffoldAuthenticationResult>();
};
const send = async (room: DurableObjectStub, frames: string[]) => {
  const response = await room.fetch("https://consumer.internal/authority/send", { method: "POST", body: JSON.stringify(frames) });
  expect(response.status).toBe(204);
};

afterEach(async () => {
  await Promise.all(sockets.splice(0).map(async ({ socket, room }) => {
    socket.close();
    // A client close event is not proof that the DO's close handler has finished.
    await expect.poll(() => runInDurableObject(room, (_instance, state) => state.storage.get("socketClosed"))).toBe(true);
    // Drain the consumer's remaining events/waitUntil work while Vitest's scope is live.
    await evictDurableObject(room, { webSockets: "close" });
  }));
  await Promise.all(tokens.splice(0).map(async token => {
    const response = await fetch(controlUrl(token), { method: "DELETE" });
    await response.text();
  }));
});

it("bounds continuous cross-DO verification waves and freshly verifies sequential batches", async () => {
  const token = await newToken();
  const rooms = Array.from({ length: 4 }, consumer);
  await Promise.all(rooms.map(async (room, index) => {
    await pause(index * 25);
    for (let wave = 0; wave < 3; wave++) {
      const result = await check(room, token);
      expect(result.status).toBe("authenticated");
      if (result.status === "authenticated") {
        expect(result.identity).toMatchObject({
          userId: "owner@example.com", projectScope: "canary", deploymentId: "canary",
          capabilities: ["session.read"], credential: "scaffold"
        });
      }
    }
  }));
  const completed = await observed(token);
  expect(completed.requests.length).toBeGreaterThanOrEqual(3);
  expect(completed.requests.length).toBeLessThanOrEqual(4);
  expect(completed.requests.every(request => request.url === "https://authority.example/api/code-sandboxes/auth/session")).toBe(true);

  // Completed successes (and explicit rejections) are not a positive or negative TTL.
  await upstream(token, 401);
  for (let wave = 1; wave <= 2; wave++) {
    expect(await check(rooms[wave], token)).toEqual({ status: "invalid" });
    const state = await observed(token);
    expect(state.requests).toHaveLength(completed.requests.length + wave);
  }

  const name = await scaffoldVerificationName(runtimeEnv, token);
  const coordinator = env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(name));
  await runInDurableObject(coordinator, async (_instance, state) => {
    expect((await state.storage.list()).size).toBe(0);
  });
}, 10_000);

it("keeps same-isolate dispatch bursts live through the real coordinator binding and recovers capacity", async () => {
  const token = await newToken();
  // Dispatch synchronously from one isolate before any coordinator can enter.
  const burst = () => Promise.all(Array.from({ length: 64 }, () => verifyScaffoldThroughCoordinator(runtimeEnv, token)));
  const first = await burst();
  expect(first.slice(0, 16).every(result => result.status === "authenticated")).toBe(true);
  expect(first.slice(16)).toEqual(Array.from({ length: 48 }, () => ({ status: "unavailable", retryAfter: "1" })));
  const completed = (await observed(token)).requests.length;
  expect(completed).toBeGreaterThanOrEqual(1);
  await upstream(token, 401);
  const recovered = await burst();
  expect(recovered.slice(0, 16)).toEqual(Array.from({ length: 16 }, () => ({ status: "invalid" })));
  expect(recovered.slice(16)).toEqual(Array.from({ length: 48 }, () => ({ status: "unavailable", retryAfter: "1" })));
  expect((await observed(token)).requests.length).toBeGreaterThan(completed);
}, 10_000);

it("shares HTTP, inbound socket, and outbound batch checks without delivering to revoked silent readers", async () => {
  const token = await newToken();
  const rooms = Array.from({ length: 4 }, consumer);
  const readers = await Promise.all(rooms.slice(0, 3).map(async room => {
    const response = await room.fetch("https://consumer.internal/authority/ws", {
      headers: { authorization: `Bearer ${token}`, upgrade: "websocket" }
    });
    expect(response.status).toBe(101);
    const socket = response.webSocket!;
    socket.accept();
    sockets.push({ socket, room });
    const frames: string[] = [];
    const closed: number[] = [];
    socket.addEventListener("message", event => { frames.push(String(event.data)); });
    socket.addEventListener("close", event => { closed.push(event.code); });
    return { socket, frames, closed };
  }));
  expect((await observed(token)).requests).toHaveLength(1);
  readers[0].socket.send("accepted");
  const http = check(rooms[3], token);
  await Promise.all(rooms.slice(0, 3).map(room => send(room, ["first", "second"])));
  expect((await http).status).toBe("authenticated");
  await expect.poll(() => readers.map(reader => reader.frames)).toEqual(Array.from({ length: 3 }, () => ["first", "second"]));
  await expect.poll(() => runInDurableObject(rooms[0], (_instance, state) => state.storage.get("authorizedMessage"))).toBe("accepted");
  const accepted = await observed(token);
  expect(accepted.requests).toHaveLength(2);

  await upstream(token, 401);
  readers[0].socket.send("forbidden");
  const revokedHttp = check(rooms[3], token);
  // Readers 1 and 2 never send anything, including after revocation.
  await Promise.all(rooms.slice(0, 3).map(room => send(room, ["must-not-arrive", "nor-this"])));
  expect(await revokedHttp).toEqual({ status: "invalid" });
  await expect.poll(() => readers.map(reader => reader.closed)).toEqual([[4401], [4401], [4401]]);
  expect(readers.map(reader => reader.frames)).toEqual(Array.from({ length: 3 }, () => ["first", "second"]));
  expect(await runInDurableObject(rooms[0], (_instance, state) => state.storage.get("authorizedMessage"))).toBe("accepted");
  expect((await observed(token)).requests).toHaveLength(3);
}, 10_000);

it.each([
  [429, "delta"], [429, "date"], [503, "delta"], [503, "date"], [503, "default"]
] as const)("holds %s unavailability until its %s deadline without delaying another token", async (status, kind) => {
  const token = await newToken(), otherToken = await newToken();
  const rooms = Array.from({ length: 3 }, consumer);
  const dateDeadline = Math.ceil(Date.now() / 1000) * 1000 + 2000;
  const retryAfter = kind === "delta" ? "2" : kind === "date" ? new Date(dateDeadline).toUTCString() : undefined;
  await upstream(token, status, retryAfter);
  const first = await Promise.all(rooms.map(room => check(room, token)));
  const firstReceivedAt = Date.now();
  for (const result of first) expect(result).toMatchObject(retryAfter === undefined ? { status: "unavailable" } : { status: "unavailable", retryAfter });
  const initial = await observed(token);
  // A bare 503 retains the existing bounded two-attempt upstream recovery.
  expect(initial.requests).toHaveLength(kind === "default" ? 2 : 1);
  const failedAt = initial.requests[initial.requests.length - 1].at;
  const deadline = kind === "date" ? dateDeadline : failedAt + (kind === "delta" ? 2000 : 1000);
  // The coordinator established cooldown before delivering these replies.
  // Keep this initial upper bound fixed: polling must not postpone recovery.
  const recoveryDeadline = kind === "date" ? dateDeadline : firstReceivedAt + (kind === "delta" ? 2000 : 1000);
  expect((await check(rooms[1], otherToken)).status).toBe("authenticated");
  const other = await observed(otherToken);
  expect(other.requests).toHaveLength(1);
  expect(other.requests[0].at).toBeLessThan(deadline);

  // Transport may cross expiry: authentication requires a fresh upstream fetch,
  // and every such fetch must respect the original upstream minimum deadline.
  await upstream(token, 200);
  let latest = initial;
  const checkWave = async (waveRooms: DurableObjectStub[]) => {
    const results = await Promise.all(waveRooms.map(room => check(room, token)));
    const current = await observed(token);
    const freshRequests = current.requests.slice(latest.requests.length);
    for (const request of freshRequests) expect(request.at).toBeGreaterThanOrEqual(deadline);
    for (const result of results) {
      expect(["unavailable", "authenticated"]).toContain(result.status);
      if (result.status === "authenticated") expect(freshRequests.length).toBeGreaterThan(0);
      if (result.status === "unavailable") expect(Number(result.retryAfter)).toBeGreaterThan(0);
    }
    latest = current;
  };
  for (let wave = 0; wave < 3; wave++) {
    await checkWave(rooms);
    await pause(40);
  }
  await pause(Math.max(0, deadline - Date.now() - 100));
  await checkWave([rooms[2]]);
  await pause(Math.max(0, recoveryDeadline - Date.now()));
  expect((await check(consumer(), token)).status).toBe("authenticated");
  const recovered = await observed(token);
  expect(recovered.requests).toHaveLength(latest.requests.length + 1);
  expect(recovered.requests[recovered.requests.length - 1].at).toBeGreaterThanOrEqual(deadline);

  // A completed recovery is not cached, even if no boundary request recovered.
  await upstream(token, 401);
  expect(await check(consumer(), token)).toEqual({ status: "invalid" });
  const revoked = await observed(token);
  expect(revoked.requests).toHaveLength(recovered.requests.length + 1);
  expect(revoked.requests[revoked.requests.length - 1].at).toBeGreaterThanOrEqual(deadline);
}, 10_000);

it("fails closed on caller configuration mismatch and never exposes the coordinator through auth routes", async () => {
  const token = await newToken();
  const room = consumer();
  for (const overrides of [
    { SCAFFOLD_CONTROL_PLANE_URL: "https://other-authority.example" },
    { SCAFFOLD_PROJECT_SCOPE: "other" },
    { SCAFFOLD_REQUIRED_CAPABILITIES: "session.control" },
    { ENVIRONMENT: "canary" },
    { SCAFFOLD_DEPLOYMENT_ID: "other" },
    { AUTH_GRANTS: null } as unknown as Partial<Env>
  ]) {
    expect((await check(room, token, overrides)).status).toBe("unavailable");
  }
  const wrongCoordinator = env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(crypto.randomUUID()));
  const direct = await wrongCoordinator.fetch("https://grant.internal/verify-scaffold", {
    method: "POST", body: JSON.stringify({ token })
  });
  expect(await direct.json()).toMatchObject({ status: "unavailable" });
  expect((await observed(token)).requests).toHaveLength(0);

  const result = await check(room, token);
  expect(result.status).toBe("authenticated");
  if (result.status !== "authenticated") throw new Error("Expected verified identity");
  for (const path of ["/verify-scaffold", "/auth/verify-scaffold"]) {
    const request = new Request(`https://edge.example${path}`, {
      method: "POST", headers: { authorization: `Bearer ${token}` }, body: JSON.stringify({ token })
    });
    // Exercise both production auth dispatchers; the fixture omits unrelated WASM routes.
    expect(await handlePublicAuthRoute(request, runtimeEnv, new URL(request.url))).toBeUndefined();
    expect(await handleAuthenticatedAuthRoute(request, runtimeEnv, new URL(request.url), result.identity)).toBeUndefined();
    const response = await SELF.fetch(request);
    expect(response.status).toBe(404);
    await response.text();
  }
  expect((await observed(token)).requests).toHaveLength(1);
}, 10_000);

it("enforces coordinator JSON and UTF-8 token ceilings on the real binding", async () => {
  const token = await newToken();
  const name = await scaffoldVerificationName(runtimeEnv, token);
  const coordinator = env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(name));
  const base = JSON.stringify({ token, padding: "" });
  const exact = JSON.stringify({ token, padding: "x".repeat(SCAFFOLD_JSON_BYTES - new TextEncoder().encode(base).byteLength) });
  const call = (body: string) => coordinator.fetch("https://grant.internal/verify-scaffold", { method: "POST", body });
  expect(await (await call(`${exact} `)).json()).toEqual({ status: "unavailable", retryAfter: "1" });
  expect(await (await call(JSON.stringify({ token: `sc_rc_${"é".repeat((SCAFFOLD_TOKEN_BYTES - 6) / 2)}x` }))).json()).toEqual({ status: "unavailable", retryAfter: "1" });
  expect((await observed(token)).requests).toHaveLength(0);
  expect(await (await call(exact)).json()).toMatchObject({ status: "authenticated" });
  await upstream(token, 401);
  expect(await (await call(exact)).json()).toEqual({ status: "invalid" });
  expect((await observed(token)).requests).toHaveLength(2);
});
