import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER, type Env } from "../../src/env";
import { AUTHORITY_HEADER, trustedRequest } from "../../src/authority";
import { authenticateResult } from "../../src/auth";
import type { Op } from "../../src/registry-core";

const authority = { userId: "user", projectId: "canary", deploymentId: "canary", credential: "scaffold", capabilities: ["session.read"], writable: true, token: "test-boundary" };
const H = { [AUTH_USER_HEADER]: "user", [AUTHORITY_HEADER]: JSON.stringify(authority) };
let tick = 1;
const hlc = () => `${String(tick++).padStart(13, "0")}-000000-mac`;
const session = (status: string, turn: string | null, updatedAt = Date.now()): Op => ({
  kind: "sessions",
  id: "c1",
  op: "upsert",
  hlc: hlc(),
  set: { chatId: "c1", deviceId: "mac", status, lastCompletedTurn: turn, updatedAt }
});

function room() {
  return env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(crypto.randomUUID()));
}
async function push(stub: DurableObjectStub, ops: Op[], headers: Record<string, string> = H) {
  const r = await stub.fetch("https://registry/push?device=mac", { method: "POST", headers, body: JSON.stringify({ batch: crypto.randomUUID(), ops }) });
  expect(r.status).toBe(200);
}
async function stats(stub: DurableObjectStub, headers: Record<string, string> = H) {
  const r = await stub.fetch("https://registry/stats", { headers });
  return r.json<{ pushTargets: Array<{ device: string; prefs: Record<string, boolean> }>; pushConfigured: boolean; pushLog: Array<{ chatId: string; category: string; device: string; result: string }> }>();
}
async function register(stub: DurableObjectStub, device: string, prefs?: Record<string, boolean>, headers: Record<string, string> = H) {
  return stub.fetch(`https://registry/push-target?device=${device}`, {
    method: "POST",
    headers,
    body: JSON.stringify({ token: "ab".repeat(32), environment: "production", prefs })
  });
}

describe("session notifications on a real RegistryRoom", () => {
  it("registers, updates and removes a phone", async () => {
    const stub = room();
    expect((await register(stub, "ios-1")).status).toBe(200);
    expect((await register(stub, "ios-1", { done: false })).status).toBe(200);
    let s = await stats(stub);
    expect(s.pushTargets).toEqual([{ device: "ios-1", environment: "production", prefs: { done: false, input: true, failed: true } }]);
    const bad = await stub.fetch("https://registry/push-target?device=ios-1", { method: "POST", headers: H, body: JSON.stringify({ token: "nope", environment: "production" }) });
    expect(bad.status).toBe(400);
    await stub.fetch("https://registry/push-target?device=ios-1", { method: "DELETE", headers: H });
    s = await stats(stub);
    expect(s.pushTargets).toEqual([]);
  });

  it("decides one notification per transition, honoring the phone's choices", async () => {
    const stub = room();
    await register(stub, "ios-1", { failed: false });
    await push(stub, [
      { kind: "chats", id: "c1", op: "upsert", hlc: hlc(), set: { title: "Fix login" } },
      session("working", null)
    ]);
    // Heartbeat: nothing. Question, then completion: one each. Failure: off.
    await push(stub, [session("working", null)]);
    await push(stub, [session("awaitingInput", null)]);
    await push(stub, [session("working", null)]);
    await push(stub, [session("idle", "turn-1")]);
    await push(stub, [session("idle", "turn-1")]);
    await push(stub, [session("errored", "turn-1")]);
    const s = await stats(stub);
    // No APNs key in tests: decided and logged, not sent.
    expect(s.pushConfigured).toBe(false);
    expect(s.pushLog.map((e) => [e.category, e.device, e.result])).toEqual([
      ["input", "ios-1", "not configured"],
      ["done", "ios-1", "not configured"]
    ]);
  });

  it("notifies for owner events through production authentication and trusted headers", async () => {
    const token = `sc_rc_${crypto.randomUUID()}`;
    const control = `https://authority-test.internal/?token=${encodeURIComponent(token)}`;
    await fetch(control, { method: "POST", body: JSON.stringify({ status: 200 }) });
    try {
      const incoming = new Request("https://registry/push", {
        headers: { authorization: `Bearer ${token}` }
      });
      const result = await authenticateResult({ ...env, AUTH_MODE: "scaffold" } as unknown as Env, incoming);
      expect(result.status).toBe("authenticated");
      if (result.status !== "authenticated") throw new Error("owner authentication failed");
      const request = trustedRequest(incoming, result.identity);
      const headers = Object.fromEntries(request.headers);
      headers[AUTH_USER_HEADER] = result.identity.userId;
      const stub = room();
      expect((await register(stub, "ios-owner", { failed: false }, headers)).status).toBe(200);
      await push(stub, [session("working", null)], headers);
      await push(stub, [session("awaitingInput", null)], headers);
      await push(stub, [session("working", null)], headers);
      await push(stub, [session("idle", "owner-turn")], headers);
      await push(stub, [session("idle", "owner-turn")], headers);
      await push(stub, [session("errored", "owner-turn")], headers);
      expect((await stats(stub, headers)).pushLog.map(e => [e.category, e.device, e.result])).toEqual([
        ["input", "ios-owner", "not configured"],
        ["done", "ios-owner", "not configured"]
      ]);
    } finally {
      await fetch(control, { method: "DELETE" });
    }
  });

  it("never notifies without authoritative account, project and deployment scope", async () => {
    for (const scope of [undefined, { ...authority, userId: "" }, { ...authority, projectId: "other" }, { ...authority, deploymentId: "other" }, { ...authority, deploymentId: "" }, { ...authority, credential: "dev", deploymentId: undefined }]) {
      const stub = room();
      await register(stub, "ios-1");
      await push(stub, [
        { kind: "chats", id: "c1", op: "upsert", hlc: hlc(), set: { title: "Fix login", userId: "forged", projectScope: "other", deploymentId: "other" } },
        session("working", null)
      ]);
      const headers: Record<string, string> = { [AUTH_USER_HEADER]: "user" };
      if (scope) headers[AUTHORITY_HEADER] = JSON.stringify(scope);
      await push(stub, [session("idle", "done")], headers);
      expect((await stats(stub)).pushLog).toEqual([]);
    }
  });

  it("stays silent for side chats and before anyone registers", async () => {
    const stub = room();
    await push(stub, [
      { kind: "chats", id: "c1", op: "upsert", hlc: hlc(), set: { title: "Side", parentChatId: "p" } },
      session("working", null)
    ]);
    await register(stub, "ios-1");
    await push(stub, [session("idle", "t")]);
    expect((await stats(stub)).pushLog).toEqual([]);
  });
});
