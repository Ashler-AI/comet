import { afterEach, describe, expect, it, vi } from "vitest";
import { authenticateResult, type Verified } from "./auth";
import { AuthGrant, authenticateDeviceToken, handleAuthenticatedAuthRoute, handlePublicAuthRoute } from "./auth-routes";
import { authorizeRoute, requestAuthority, scopedRpcAllowed, socketAuthorized, sendAuthorized, stampRpcAuthority, trustedRequest } from "./authority";
import type { Env } from "./env";

const identity: Verified = { userId: "owner@example.com", email: "owner@example.com", projectScope: "ashler", orgId: "ashler", deploymentId: "staging", capabilities: ["session.read", "session.chat", "session.control", "session.environment", "session.sync.write"], credential: "scaffold" };
const device: Verified = { ...identity, orgId: undefined, credential: "device", sessionId: "session-a", targetDeviceId: "device-a", capabilities: ["session.read", "session.chat"] };
const config = { AUTH_MODE: "scaffold", ENVIRONMENT: "crew-upstream-canary", SCAFFOLD_PROJECT_SCOPE: "ashler", SCAFFOLD_DEPLOYMENT_ID: "staging", SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.example", SCAFFOLD_REQUIRED_CAPABILITIES: "session.read" };
const req = (path: string, method = "GET") => new Request(`https://edge.example${path}`, { method });
afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe("scoped transport authority", () => {
  it("keeps the personal registry per user, never per sandbox", () => {
    expect(authorizeRoute(req("/registry/ashler/ws"), identity)).toBe(true);
    for (const path of ["/registry/ashler/ws", "/registry/ashler/rows", "/registry/ashler/push", "/workspace/ashler/ws", "/device/other/ws?role=host", "/chat2/other/rows", "/blob/other/tool"]) {
      expect(authorizeRoute(req(path), device), path).toBe(false);
    }
  });
  it("keeps headless artifact updates authenticated without granting user authority", () => {
    expect(authorizeRoute(req("/api/releases/manifest.json"), device)).toBe(true);
    expect(authorizeRoute(req("/api/releases/manifest.json", "PUT"), device)).toBe(false);
    expect(authorizeRoute(req("/api/releases/manifest.json"), { ...device, capabilities: [] })).toBe(false);
  });
  it("denies wrong project/deployment and opaque writes but preserves granted reads", () => {
    expect(authorizeRoute(req("/chat2/session-a/rows"), device)).toBe(true);
    expect(authorizeRoute(req("/chat2/session-a/ws"), device)).toBe(true);
    expect(authorizeRoute(req("/chat2/session-a/rows?deploymentId=prod"), device)).toBe(false);
    expect(authorizeRoute(req("/chat2/session-a/rows?projectId=other"), device)).toBe(false);
    expect(authorizeRoute(req("/chat2/session-a/rows", "POST"), device)).toBe(false);
    expect(authorizeRoute(req("/chat2/session-a/checkpoint", "POST"), device)).toBe(false);
    expect(authorizeRoute(req("/chat2/session-a/rows", "POST"), { ...device, capabilities: [...device.capabilities, "session.sync.write"] })).toBe(true);
    expect(authorizeRoute(req("/device/device-a/ws?role=host"), device)).toBe(false);
    expect(authorizeRoute(req("/device/device-a/ws?role=client"), device)).toBe(true);
  });
  it("strips forged authority and removes bearer from forwarded URL/header", () => {
    const forwarded = trustedRequest(new Request("https://edge.example/chat2/session-a/ws?token=secret", { headers: { "x-zeron-auth-user": "attacker", "x-zeron-room-kind": "workspace", "x-crew-authority": "forged", "x-comet-capabilities": "everything" } }), device);
    expect(forwarded.headers.has("x-zeron-auth-user")).toBe(false);
    expect(forwarded.headers.has("x-zeron-room-kind")).toBe(false);
    expect(forwarded.headers.has("x-comet-capabilities")).toBe(false);
    expect(new URL(forwarded.url).searchParams.has("token")).toBe(false);
    expect(requestAuthority(forwarded)?.userId).toBe(identity.userId);
    expect(requestAuthority(forwarded)?.writable).toBe(false);
  });
  it("preserves exact-session semantic commands, rejects opaque RPC and terminal escape", () => {
    const authority = requestAuthority(trustedRequest(req("/device/device-a/ws"), device));
    const payload = (method: string, chatId = "session-a", kind = "run") => new TextEncoder().encode(JSON.stringify({ method, params: { chatId, command: { kind } } }));
    expect(scopedRpcAllowed("rpc", payload("QueueCommand"), authority)).toBe(true);
    expect(scopedRpcAllowed("rpc", payload("QueueCommand", "other"), authority)).toBe(false);
    expect(scopedRpcAllowed("rpc", payload("QueueCommand", "session-a", "interrupt"), authority)).toBe(false);
    expect(scopedRpcAllowed("rpc", payload("ReadFile"), authority)).toBe(false);
    expect(scopedRpcAllowed("rpc", payload("scaffold.control"), authority)).toBe(false);
    expect(scopedRpcAllowed("term", payload("QueueCommand"), authority)).toBe(false);
    expect(scopedRpcAllowed("rpc", payload("ListCrewAnnotations"), authority)).toBe(true);
    expect(scopedRpcAllowed("rpc", payload("ApplyCrewAnnotation"), authority)).toBe(false);
    const annotator = { ...authority!, capabilities: [...authority!.capabilities, "session.annotate"] };
    expect(scopedRpcAllowed("rpc", payload("ApplyCrewAnnotation"), annotator)).toBe(true);
    expect(scopedRpcAllowed("rpc", payload("ApplyCrewAnnotation", "other"), annotator)).toBe(false);
  });
  it("admits empty liveness probes without granting a data channel or extra capability", () => {
    const authority = requestAuthority(trustedRequest(req("/device/device-a/ws"), device))!;
    expect(scopedRpcAllowed("echo", new Uint8Array(), authority)).toBe(true);
    expect(scopedRpcAllowed("echo", new Uint8Array([1]), authority)).toBe(false);
    expect(scopedRpcAllowed("echo", new Uint8Array(), { ...authority, capabilities: [] })).toBe(false);
  });
  it("admits scoped upload bytes only for the exact session and existing files capability", () => {
    const authority = requestAuthority(trustedRequest(req("/device/device-a/ws"), device))!;
    const files = { ...authority, capabilities: [...authority.capabilities, "session.files"] };
    for (const method of ["UploadChunk", "UploadCommit"]) {
      for (const chatId of ["session-a", "other", undefined]) {
        const payload = new TextEncoder().encode(JSON.stringify({ method, params: { chatId, uploadId: "upload", data: "eA==", seq: 0, fileName: "image.png" } }));
        expect(scopedRpcAllowed("rpc", payload, files)).toBe(chatId === "session-a");
        expect(scopedRpcAllowed("rpc", payload, authority)).toBe(false);
        expect(scopedRpcAllowed("term", payload, files)).toBe(false);
      }
    }
    for (const method of ["LocalDevice", "ListHarnesses", "ReadFile", "WriteFile", "ReadAttachmentChunk"]) {
      const payload = new TextEncoder().encode(JSON.stringify({ method, params: {} }));
      expect(scopedRpcAllowed("rpc", payload, files)).toBe(method === "LocalDevice");
    }
  });
  it("overwrites forged annotation authority with nonsecret verified relay identity", () => {
    const authority = requestAuthority(trustedRequest(new Request("https://edge.example/device/device-a/ws?token=secret"), device));
    const payload = new TextEncoder().encode(JSON.stringify({
      id: 7, method: "ListCrewAnnotations", params: {
        chatId: "session-a", __crewAuthority: { subject: "attacker", capabilities: ["session.annotate"], token: "forged" }
      }
    }));
    const stamped = JSON.parse(new TextDecoder().decode(stampRpcAuthority(payload, authority)));
    expect(stamped.params.__crewAuthority).toEqual({
      subject: device.userId, projectId: device.projectScope, deploymentId: device.deploymentId,
      sessionId: device.sessionId, deviceId: device.targetDeviceId, capabilities: device.capabilities
    });
    expect(stamped.params.__crewAuthority).not.toHaveProperty("token");
    expect(stamped.id).toBe(7);
    expect(JSON.parse(new TextDecoder().decode(stampRpcAuthority(payload))).params).not.toHaveProperty("__crewAuthority");
  });
  it("distinguishes authority failure from rejection and fails closed outside local dev", async () => {
    const request = new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } });
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 503 })));
    expect((await authenticateResult(config as Env, request)).status).toBe("unavailable");
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response("", { status: 401 })));
    expect((await authenticateResult(config as Env, request)).status).toBe("invalid");
    expect((await authenticateResult({ ...config, AUTH_MODE: "dev" } as Env, request)).status).toBe("unavailable");
  });
  it("binds server-verified resource and principal, never caller identity headers", async () => {
    const fetcher = vi.fn().mockResolvedValue(Response.json({ ok: true, actor: { sub: "Owner@Example.com", auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read", "remote_code:exec"] }));
    vi.stubGlobal("fetch", fetcher);
    const result = await authenticateResult(config as Env, new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test", "x-zeron-auth-user": "forged" } }));
    expect(result.status === "authenticated" && result.identity.userId).toBe(identity.userId);
    expect(fetcher.mock.calls[0][1].redirect).toBe("manual");
  });
  it("preserves backfill order with one check per batch and blocks later delivery after revocation", async () => {
    const fetcher = vi.fn().mockImplementation(async () => Response.json({ ok: true, actor: { sub: identity.userId, auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"] }));
    vi.stubGlobal("fetch", fetcher);
    const send = vi.fn(), close = vi.fn();
    const authority = requestAuthority(trustedRequest(new Request("https://edge.example/?token=sc_rc_test"), { ...identity, capabilities: ["session.read"] }));
    const socket = { deserializeAttachment: () => ({ authority }), send, close } as unknown as WebSocket;
    const work: Promise<unknown>[] = [];
    const ctx = { waitUntil: (promise: Promise<unknown>) => work.push(promise) } as unknown as DurableObjectState;
    sendAuthorized(config as Env, ctx, socket, "first");
    sendAuthorized(config as Env, ctx, socket, "second");
    await Promise.all(work);
    expect(send.mock.calls).toEqual([["first"], ["second"]]);
    expect(fetcher).toHaveBeenCalledTimes(1);
    fetcher.mockImplementation(async () => new Response("", { status: 401 }));
    sendAuthorized(config as Env, ctx, socket, "revoked");
    await Promise.all(work);
    expect(send).toHaveBeenCalledTimes(2);
    expect(close).toHaveBeenCalledWith(4401, "authority expired or revoked");
  });
});

describe("device grant lifecycle", () => {
  it("fails closed when the native internal view has no epoch binding", async () => {
    const ctx = { blockConcurrencyWhile: (fn: () => Promise<Response>) => fn() } as unknown as DurableObjectState;
    const grant = new AuthGrant(ctx, { NATIVE_GRANT_EPOCHS: undefined } as unknown as Env);
    for (const path of ["issue", "exchange", "refresh", "authenticate", "native-epoch/advance", "native-epoch/check"]) {
      const response = await grant.fetch(new Request(`https://grant.internal/${path}`, { method: "POST" }));
      expect(response.status).toBe(503);
      expect(await response.json()).toEqual({ error: "epoch_authority_unavailable" });
    }
  });
  function setup() {
    const grants = new Map<string, AuthGrant>();
    const ns = { idFromName: (id: string) => id, get: (id: string) => {
      let grant = grants.get(id);
      if (!grant) {
        const values = new Map<string, unknown>();
        let serial = Promise.resolve();
        grant = new AuthGrant({ storage: { get: async (key: string) => structuredClone(values.get(key)), put: async (key: string, value: unknown) => { values.set(key, structuredClone(value)); } }, blockConcurrencyWhile: (fn: () => Promise<unknown>) => { const result = serial.then(fn); serial = result.then(() => undefined, () => undefined); return result; } } as unknown as DurableObjectState, {} as Env);
        grants.set(id, grant);
      }
      return { fetch: (input: string | Request, init?: RequestInit) => grant!.fetch(input instanceof Request ? input : new Request(input, init)) };
    } };
    const env = { ...config, AUTH_GRANTS: ns, CHAT_ROOMS: { idFromName: (id: string) => id, get: () => ({ fetch: async () => Response.json({ ok: true }) }) } } as unknown as Env;
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ ok: true, profile: { version: "scaffold.comet-runtime.v1", projectId: "ashler", deploymentId: "staging", sandboxId: "sandbox-a", targetDeviceId: "device-a", sessionId: "session-a", lifecycleEpoch: 1, actor: { sub: identity.userId } } })));
    return env;
  }
  async function issue(env: Env) {
    const request = new Request("https://edge.example/auth/device-grants", { method: "POST", headers: { authorization: "Bearer sc_rc_owner" }, body: JSON.stringify({ deploymentId: "staging", sandboxId: "sandbox-a", targetDeviceId: "device-a", sessionId: "session-a", lifecycleEpoch: 1, capabilities: ["session.read", "session.sync.write"] }) });
    const response = await handleAuthenticatedAuthRoute(request, env, new URL(request.url), identity);
    expect(response?.status).toBe(200);
    return (await response!.json()) as { grant: string };
  }
  it("exchanges once, rotates without changing scope, revokes established sockets", async () => {
    const env = setup();
    const { grant } = await issue(env);
    const exchange = () => { const r = new Request("https://edge.example/auth/device-grants/exchange", { method: "POST", body: JSON.stringify({ grant }) }); return handlePublicAuthRoute(r, env, new URL(r.url)); };
    const responses = await Promise.all([exchange(), exchange()]);
    expect(responses.map(r => r!.status).sort()).toEqual([200, 401]);
    const access = await responses.find(r => r!.status === 200)!.json() as { accessToken: string };
    const verified = await authenticateDeviceToken(env, access.accessToken);
    expect(verified?.sessionId).toBe("session-a");
    const refresh = new Request("https://edge.example/auth/device-token/refresh", { method: "POST", headers: { authorization: `Bearer ${access.accessToken}` } });
    const rotated = await (await handleAuthenticatedAuthRoute(refresh, env, new URL(refresh.url), verified!))!.json() as { accessToken: string; lifecycleEpoch: number };
    expect(rotated.lifecycleEpoch).toBe(1);
    expect(await authenticateDeviceToken(env, access.accessToken)).toBeUndefined();
    const current = await authenticateDeviceToken(env, rotated.accessToken);
    expect(current?.sessionId).toBe("session-a");
    expect(await authenticateDeviceToken({ ...env, SCAFFOLD_DEPLOYMENT_ID: "prod" }, rotated.accessToken)).toBeUndefined();
    const close = vi.fn();
    const authority = requestAuthority(trustedRequest(new Request(`https://edge.example/?token=${rotated.accessToken}`), current!));
    const socket = { deserializeAttachment: () => ({ authority }), close } as unknown as WebSocket;
    expect(await socketAuthorized(env, socket)).toBe(true);
    for (const field of ["projectId", "deploymentId", "sessionId", "deviceId", "grantId", "sandboxId"] as const) {
      const changed = { deserializeAttachment: () => ({ authority: { ...authority, [field]: "other" } }), close } as unknown as WebSocket;
      expect(await socketAuthorized(env, changed), field).toBe(false);
    }
    const staleEpoch = { deserializeAttachment: () => ({ authority: { ...authority, lifecycleEpoch: 2 } }), close } as unknown as WebSocket;
    expect(await socketAuthorized(env, staleEpoch)).toBe(false);
    const clock = vi.spyOn(Date, "now").mockReturnValue(Date.now() + 13 * 60 * 60 * 1000);
    expect(await authenticateDeviceToken(env, rotated.accessToken)).toBeUndefined();
    expect(await socketAuthorized(env, socket)).toBe(false);
    clock.mockRestore();
    const unavailable = { ...env, AUTH_GRANTS: { idFromName: (id: string) => id, get: () => ({ fetch: async () => { throw new Error("unavailable"); } }) } } as unknown as Env;
    expect(await socketAuthorized(unavailable, socket)).toBe(false);
    expect(close).toHaveBeenCalledWith(1013, "authority unavailable");
    const revoke = new Request(`https://edge.example/auth/device-grants?id=${current!.grantId}`, { method: "DELETE" });
    expect((await handleAuthenticatedAuthRoute(revoke, env, new URL(revoke.url), identity))?.status).toBe(200);
    expect(await socketAuthorized(env, socket)).toBe(false);
    expect(close).toHaveBeenCalledWith(4401, "authority expired or revoked");
  });
  it("rejects caller-invented sandbox ownership and recursive grants", async () => {
    const env = setup();
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ ok: true, profile: { actor: { sub: "another@example.com" } } })));
    const request = new Request("https://edge.example/auth/device-grants", { method: "POST", headers: { authorization: "Bearer sc_rc_owner" }, body: JSON.stringify({ deploymentId: "staging", sandboxId: "sandbox-a", targetDeviceId: "device-a", sessionId: "session-a", lifecycleEpoch: 1, capabilities: ["session.read"] }) });
    expect((await handleAuthenticatedAuthRoute(request, env, new URL(request.url), identity))?.status).toBe(403);
    expect((await handleAuthenticatedAuthRoute(req("/auth/device-grants", "POST"), env, new URL(request.url), device))?.status).toBe(403);
  });
});
