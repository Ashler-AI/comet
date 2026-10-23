import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { authenticateResult, type Verified } from "./auth";
import { AuthGrant, authenticateDeviceToken, handleAuthenticatedAuthRoute, handlePublicAuthRoute, scaffoldVerificationName, verifyScaffoldThroughCoordinator } from "./auth-routes";
import { boundedAuthJson, scaffoldAdmission, SCAFFOLD_JSON_BYTES, SCAFFOLD_TOKEN_BYTES } from "./auth-envelope";
import { authorizeRoute, requestAuthority, scopedRpcAllowed, socketAuthorized, sendAuthorized, stampRpcAuthority, trustedRequest } from "./authority";
import type { Env } from "./env";

const identity: Verified = { userId: "owner@example.com", email: "owner@example.com", projectScope: "ashler", orgId: "ashler", deploymentId: "staging", capabilities: ["session.read", "session.chat", "session.control", "session.environment", "session.sync.write"], credential: "scaffold" };
const device: Verified = { ...identity, orgId: undefined, credential: "device", sessionId: "session-a", targetDeviceId: "device-a", capabilities: ["session.read", "session.chat"] };
const baseConfig = { AUTH_MODE: "scaffold", ENVIRONMENT: "crew-upstream-canary", SCAFFOLD_PROJECT_SCOPE: "ashler", SCAFFOLD_DEPLOYMENT_ID: "staging", SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.example", SCAFFOLD_REQUIRED_CAPABILITIES: "session.read" };
const authorityEnv = (overrides: Partial<Env> = {}): Env => {
  const grants = new Map<string, AuthGrant>();
  const env = { ...baseConfig, ...overrides } as Env;
  env.AUTH_GRANTS = {
    idFromName: (name: string) => name,
    get: (id: string) => {
      let grant = grants.get(id);
      if (!grant) {
        grant = new AuthGrant({
          id,
          storage: { get: vi.fn(), put: vi.fn() },
          blockConcurrencyWhile: () => { throw new Error("verification must not enter grant storage"); }
        } as unknown as DurableObjectState, env);
        grants.set(id, grant);
      }
      return { fetch: (input: string | Request, init?: RequestInit) => grant!.fetch(input instanceof Request ? input : new Request(input, init)) };
    }
  } as unknown as DurableObjectNamespace;
  return env;
};
let config: Env;
beforeEach(() => { config = authorityEnv(); });
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
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 1001);
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
  it("shares only in-flight equivalent verification through decoding and rechecks revocation", async () => {
    const session = { ok: true, actor: { sub: identity.userId, auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"] };
    let release!: () => void, decodingStarted!: () => void;
    const bodyReady = new Promise<void>(resolve => { release = resolve; });
    const decoding = new Promise<void>(resolve => { decodingStarted = resolve; });
    const response = new Response(new ReadableStream({
      async pull(controller) {
        decodingStarted();
        await bodyReady;
        controller.enqueue(new TextEncoder().encode(JSON.stringify(session)));
        controller.close();
      }
    }, { highWaterMark: 0 }));
    const fetcher = vi.fn().mockResolvedValueOnce(response).mockImplementation(async (url: string, init: RequestInit) => Response.json({
      ...session,
      resource: new URL(url).origin,
      actor: { sub: new Headers(init.headers).get("authorization") === "Bearer sc_rc_other" ? "other@example.com" : identity.userId, auth: "iap" }
    }));
    vi.stubGlobal("fetch", fetcher);
    const request = (token = "sc_rc_test") => new Request("https://edge.example/auth/session", { headers: { authorization: `Bearer ${token}` } });
    const first = authenticateResult(config as Env, request());
    const concurrent = authenticateResult({ ...config } as Env, request());
    await decoding;
    const duringDecode = authenticateResult(config as Env, request());
    const otherToken = authenticateResult(config as Env, request("sc_rc_other"));
    const otherOrigin = authenticateResult({ ...config, SCAFFOLD_CONTROL_PLANE_URL: "https://other.example" } as Env, request());
    const otherProject = authenticateResult({ ...config, SCAFFOLD_PROJECT_SCOPE: "other" } as Env, request());
    const otherCapabilities = authenticateResult({ ...config, SCAFFOLD_REQUIRED_CAPABILITIES: "session.control" } as Env, request());
    const [token, origin, project, capabilities] = await Promise.all([otherToken, otherOrigin, otherProject, otherCapabilities]);
    release();
    const [verified, shared, decoded] = await Promise.all([first, concurrent, duringDecode]);
    expect(verified.status).toBe("authenticated");
    expect(shared).toEqual(verified);
    expect(decoded).toEqual(verified);
    expect(token.status === "authenticated" && token.identity.userId).toBe("other@example.com");
    expect(origin).toEqual({ status: "unavailable" });
    expect(project).toEqual({ status: "unavailable" });
    expect(capabilities).toEqual({ status: "unavailable" });
    expect(fetcher).toHaveBeenCalledTimes(2);
    fetcher.mockResolvedValue(new Response("", { status: 401 }));
    expect(await authenticateResult(config as Env, request())).toEqual({ status: "invalid" });
    expect(fetcher).toHaveBeenCalledTimes(3);
    expect(await authenticateResult(config as Env, request())).toEqual({ status: "invalid" });
    expect(fetcher).toHaveBeenCalledTimes(4);
  });
  it.each(["2", "Thu, 22 Oct 2026 12:00:02 GMT"])("shares unavailable cooldown until Retry-After %s, not beyond it", async (retryAfter) => {
    const start = Date.parse("2026-10-22T12:00:00Z");
    const clock = vi.spyOn(Date, "now").mockReturnValue(start);
    const fetcher = vi.fn().mockResolvedValueOnce(new Response("", { status: 429, headers: { "retry-after": retryAfter } }))
      .mockImplementation(async (_url: string, init: RequestInit) => new Headers(init.headers).get("authorization") === "Bearer sc_rc_other"
        ? Response.json({ ok: true, actor: { sub: "other@example.com", auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"] })
        : new Response("", { status: 401 }));
    vi.stubGlobal("fetch", fetcher);
    const request = (token = "sc_rc_test") => new Request("https://edge.example/auth/session", { headers: { authorization: `Bearer ${token}` } });
    const results = await Promise.all(Array.from({ length: 12 }, () => authenticateResult({ ...config }, request())));
    expect(results).toEqual(Array.from({ length: 12 }, () => ({ status: "unavailable", retryAfter })));
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect((await authenticateResult(config, request("sc_rc_other"))).status).toBe("authenticated");
    clock.mockReturnValue(start + 1999);
    expect(await authenticateResult({ ...config }, request())).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(fetcher).toHaveBeenCalledTimes(2);
    clock.mockReturnValue(start + 2000);
    expect(await authenticateResult({ ...config }, request())).toEqual({ status: "invalid" });
    expect(fetcher).toHaveBeenCalledTimes(3);
  });
  it.each(["9007199254740992", "9".repeat(307), "9".repeat(309)])("never shortens an unrepresentable numeric Retry-After %s", async (retryAfter) => {
    const start = Date.now();
    const clock = vi.spyOn(Date, "now").mockReturnValue(start);
    const fetcher = vi.fn().mockResolvedValue(new Response("", { status: 429, headers: { "retry-after": retryAfter } }));
    vi.stubGlobal("fetch", fetcher);
    const request = new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } });
    expect(await authenticateResult(config, request)).toEqual({ status: "unavailable", retryAfter });
    for (const now of [start + 1001, start + 86_400_000, Number.MAX_SAFE_INTEGER]) {
      clock.mockReturnValue(now);
      expect(await authenticateResult({ ...config }, request)).toEqual({ status: "unavailable", retryAfter });
    }
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it("rejects mismatched deployment/config and malformed or throwing coordinator bindings without an upstream fallback", async () => {
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    const request = new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } });
    for (const overrides of [
      { SCAFFOLD_DEPLOYMENT_ID: "other" }, { ENVIRONMENT: "other" },
      { SCAFFOLD_CONTROL_PLANE_URL: "https://other.example" },
      { SCAFFOLD_PROJECT_SCOPE: "other" }, { SCAFFOLD_REQUIRED_CAPABILITIES: "session.control" }
    ]) expect(await authenticateResult({ ...config, ...overrides }, request)).toEqual({ status: "unavailable" });
    for (const reply of [null, {}, { status: "authenticated" }, { status: "authenticated", identity: { ...identity, capabilities: [] } }]) {
      const env = { ...config, AUTH_GRANTS: { idFromName: (name: string) => name, get: () => ({ fetch: async () => Response.json(reply) }) } } as unknown as Env;
      expect(await authenticateResult(env, request)).toEqual({ status: "unavailable" });
    }
    const env = { ...config, AUTH_GRANTS: { idFromName: () => { throw new Error("binding unavailable"); } } } as unknown as Env;
    expect((await authenticateResult(env, request)).status).toBe("unavailable");
    expect(fetcher).not.toHaveBeenCalled();
  });
  it.each([
    [429, undefined, "unavailable"],
    [429, "120", "unavailable"],
    [408, "120", "unavailable"],
    [503, "Wed, 21 Oct 2037 07:28:00 GMT", "unavailable"],
    [401, "120", "invalid"],
    [403, "120", "invalid"]
  ] as const)("does not retry upstream %s with Retry-After %s", async (status, retryAfter, expected) => {
    const fetcher = vi.fn()
      .mockResolvedValueOnce(new Response("", { status, headers: retryAfter === undefined ? {} : { "retry-after": retryAfter } }))
      .mockResolvedValueOnce(Response.json({ ok: true, actor: { sub: identity.userId, auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"] }));
    vi.stubGlobal("fetch", fetcher);
    const result = await authenticateResult(config as Env, new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } }));
    expect(result).toEqual(expected === "unavailable" && retryAfter !== undefined ? { status: expected, retryAfter } : { status: expected });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it.each([408, 500, 503, "network"] as const)("recovers once from %s and revalidates scope and resource on later requests", async (failure) => {
    const session = { ok: true, actor: { sub: identity.userId, auth: "iap" }, resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"] };
    const fetcher = vi.fn();
    if (failure === "network") fetcher.mockRejectedValueOnce(new Error("network unavailable"));
    else fetcher.mockResolvedValueOnce(new Response("", { status: failure }));
    fetcher.mockResolvedValueOnce(Response.json(session))
      .mockResolvedValueOnce(Response.json({ ...session, scopes: [] }))
      .mockResolvedValueOnce(Response.json({ ...session, resource: "https://other.example" }));
    vi.stubGlobal("fetch", fetcher);
    const request = new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } });
    const result = await authenticateResult(config as Env, request);
    expect(result.status === "authenticated" && result.identity.userId).toBe(identity.userId);
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(await authenticateResult(config as Env, request)).toEqual({ status: "unavailable" });
    expect(fetcher).toHaveBeenCalledTimes(3);
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 1001);
    expect(await authenticateResult(config as Env, request)).toEqual({ status: "unavailable" });
    expect(fetcher).toHaveBeenCalledTimes(4);
  });
  it.each([408, 503, "network"] as const)("bounds persistent %s recovery to two attempts", async (failure) => {
    const fetcher = vi.fn();
    if (failure === "network") fetcher.mockRejectedValue(new Error("network unavailable"));
    else fetcher.mockResolvedValue(new Response("", { status: failure }));
    vi.stubGlobal("fetch", fetcher);
    expect(await authenticateResult(config as Env, new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } }))).toEqual({ status: "unavailable" });
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(await authenticateResult({ ...config }, new Request("https://edge.example/auth/session", { headers: { authorization: "Bearer sc_rc_test" } }))).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(fetcher).toHaveBeenCalledTimes(2);
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

describe("Scaffold coordinator resource envelope", () => {
  afterEach(() => { vi.useRealTimers(); });

  it.each([undefined, "1"])("counts streamed bytes rather than Content-Length %s and cancels overflow", async length => {
    for (const extra of [0, 1]) {
      const cancel = vi.fn();
      const bytes = new TextEncoder().encode(`"${"x".repeat(SCAFFOLD_JSON_BYTES - 2 + extra)}"`);
      let offset = 0;
      const response = new Response(new ReadableStream({
        pull(controller) {
          if (offset < bytes.length) {
            controller.enqueue(bytes.slice(offset, offset += 1024));
          } else if (!extra) controller.close();
        }, cancel
      }), { headers: length ? { "content-length": length } : {} });
      if (extra) {
        await expect(boundedAuthJson(response, new AbortController().signal)).rejects.toThrow();
        expect(cancel).toHaveBeenCalledOnce();
      } else expect((await boundedAuthJson<string>(response, new AbortController().signal)).length).toBe(SCAFFOLD_JSON_BYTES - 2);
      expect(response.body!.locked).toBe(false);
    }
  });

  it("cancels stalled decoding and releases its reader", async () => {
    const cancel = vi.fn();
    const response = new Response(new ReadableStream({ cancel }));
    const controller = new AbortController();
    const pending = boundedAuthJson(response, controller.signal);
    controller.abort();
    await expect(pending).rejects.toThrow();
    expect(cancel).toHaveBeenCalledOnce();
    expect(response.body!.locked).toBe(false);
  });

  it("applies the eight-second envelope through body reads and cleans their timer", async () => {
    vi.useFakeTimers();
    const cancel = vi.fn();
    const response = new Response(new ReadableStream({ cancel }));
    const pending = scaffoldAdmission("coordinator", undefined, async signal => {
      await boundedAuthJson(response, signal);
      return { status: "invalid" };
    });
    await vi.advanceTimersByTimeAsync(7999);
    expect(cancel).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(await pending).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(cancel).toHaveBeenCalledOnce();
    expect(response.body!.locked).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("checks UTF-8 token bytes before naming or dispatch, including the exact limit", async () => {
    const dispatch = vi.fn(async () => Response.json({ status: "invalid" }));
    const names = vi.fn((name: string) => name);
    const env = { ...config, AUTH_GRANTS: { idFromName: names, get: () => ({ fetch: dispatch }) } } as unknown as Env;
    const exact = `sc_rc_${"é".repeat((SCAFFOLD_TOKEN_BYTES - 6) / 2)}`;
    expect(await verifyScaffoldThroughCoordinator(env, exact)).toEqual({ status: "invalid" });
    expect(dispatch).toHaveBeenCalledOnce();
    expect(await verifyScaffoldThroughCoordinator(env, `${exact}x`)).toEqual({ status: "unavailable", retryAfter: "1" });
    await expect(scaffoldVerificationName(env, `${exact}x`)).rejects.toThrow();
    expect(names).toHaveBeenCalledOnce();
    expect(dispatch).toHaveBeenCalledOnce();
  });

  it.each([false, true])("reserves coordinator headroom for a 64-dispatch same-isolate burst and recovers after failure=%s", async fail => {
    let release!: () => void;
    const held = new Promise<void>(resolve => { release = resolve; });
    const authority = vi.fn(async () => {
      await held;
      if (fail) throw new Error("authority unavailable");
      return { status: "invalid" as const };
    });
    const dispatch = () => scaffoldAdmission("dispatch", undefined, async signal => {
      await Promise.resolve();
      return scaffoldAdmission("coordinator", signal, authority);
    });
    const pending = Array.from({ length: 64 }, dispatch);
    try {
      expect(await Promise.all(pending.slice(16))).toEqual(Array.from({ length: 48 }, () => ({ status: "unavailable", retryAfter: "1" })));
      expect(authority).toHaveBeenCalledTimes(16);
      const rejected = vi.fn(async () => ({ status: "invalid" as const }));
      // Sixteen outbound plus sixteen inbound calls consume the same 32 slots.
      for (const kind of ["dispatch", "coordinator"] as const) {
        expect(await scaffoldAdmission(kind, undefined, rejected)).toEqual({ status: "unavailable", retryAfter: "1" });
      }
      expect(rejected).not.toHaveBeenCalled();
    } finally {
      release();
      await Promise.all(pending);
    }
    expect(await Promise.all(pending.slice(0, 16))).toEqual(Array.from({ length: 16 }, () => fail ? { status: "unavailable", retryAfter: "1" } : { status: "invalid" }));
    const recovered = await Promise.all(Array.from({ length: 64 }, dispatch));
    expect(authority).toHaveBeenCalledTimes(32);
    expect(recovered.slice(0, 16)).toEqual(Array.from({ length: 16 }, () => fail ? { status: "unavailable", retryAfter: "1" } : { status: "invalid" }));
    expect(recovered.slice(16)).toEqual(Array.from({ length: 48 }, () => ({ status: "unavailable", retryAfter: "1" })));
  });

  it("retains all 32 admissions after cancellation until uncancellable work settles", async () => {
    let release!: () => void;
    const held = new Promise<void>(resolve => { release = resolve; });
    const controllers = Array.from({ length: 32 }, () => new AbortController());
    const pending = controllers.map((controller, index) => scaffoldAdmission(index < 16 ? "dispatch" : "coordinator", controller.signal, async () => {
      await held;
      return { status: "invalid" };
    }));
    const rejected = vi.fn(async () => ({ status: "invalid" as const }));
    expect(await scaffoldAdmission("coordinator", undefined, rejected)).toEqual({ status: "unavailable", retryAfter: "1" });
    controllers.forEach(controller => controller.abort());
    expect(await Promise.all(pending)).toEqual(Array.from({ length: 32 }, () => ({ status: "unavailable", retryAfter: "1" })));
    for (let churn = 0; churn < 64; churn++) {
      for (const kind of ["dispatch", "coordinator"] as const) {
        expect(await scaffoldAdmission(kind, undefined, rejected)).toEqual({ status: "unavailable", retryAfter: "1" });
      }
    }
    expect(rejected).not.toHaveBeenCalled();
    release();
    await held;
    await Promise.resolve();
    const recovered = Array.from({ length: 32 }, (_, index) => scaffoldAdmission(index < 16 ? "dispatch" : "coordinator", undefined, rejected));
    expect(await Promise.all(recovered)).toEqual(Array.from({ length: 32 }, () => ({ status: "invalid" })));
  });

  it("times out dispatch at eight seconds without freeing retained admissions", async () => {
    vi.useFakeTimers();
    let release!: (response: Response) => void;
    let dispatched!: () => void;
    const started = new Promise<void>(resolve => { dispatched = resolve; });
    let signal!: AbortSignal;
    const env = { ...config, AUTH_GRANTS: { idFromName: (name: string) => name, get: () => ({
      fetch: (_url: string, init: RequestInit) => { signal = init.signal!; dispatched(); return new Promise<Response>(resolve => { release = resolve; }); }
    }) } } as unknown as Env;
    const pending = verifyScaffoldThroughCoordinator(env, "sc_rc_stalled");
    await started;
    await vi.advanceTimersByTimeAsync(8000);
    expect(await pending).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(signal.aborted).toBe(true);
    release(Response.json({ status: "authenticated", identity }));
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("bounds internal request, internal result, and authority response bytes", async () => {
    const token = "sc_rc_body_bound";
    const name = await scaffoldVerificationName(config, token);
    const coordinator = config.AUTH_GRANTS.get(config.AUTH_GRANTS.idFromName(name));
    const fetcher = vi.fn(async () => new Response(`{"padding":"${"x".repeat(SCAFFOLD_JSON_BYTES)}"}`));
    vi.stubGlobal("fetch", fetcher);
    const oversized = await coordinator.fetch("https://grant.internal/verify-scaffold", {
      method: "POST", body: JSON.stringify({ token, padding: "x".repeat(SCAFFOLD_JSON_BYTES) })
    });
    expect(await oversized.json()).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(fetcher).not.toHaveBeenCalled();
    expect((await verifyScaffoldThroughCoordinator(config, token)).status).toBe("unavailable");
    expect(fetcher).toHaveBeenCalledOnce();
    const malformed = { ...config, AUTH_GRANTS: { idFromName: (name: string) => name, get: () => ({ fetch: async () => new Response(" ".repeat(SCAFFOLD_JSON_BYTES + 1)) }) } } as unknown as Env;
    expect((await verifyScaffoldThroughCoordinator(malformed, token)).status).toBe("unavailable");
  });

  it("one cancelled caller does not abort shared authority work; the last caller does", async () => {
    let started!: () => void;
    const ready = new Promise<void>(resolve => { started = resolve; });
    let authoritySignal!: AbortSignal;
    let release!: (response: Response) => void;
    const fetcher = vi.fn((_url: string, init: RequestInit) => {
      authoritySignal = init.signal!;
      started();
      return new Promise<Response>(resolve => { release = resolve; });
    });
    vi.stubGlobal("fetch", fetcher);
    const first = new AbortController(), second = new AbortController();
    const name = await scaffoldVerificationName(config, "sc_rc_cancel");
    const coordinator = config.AUTH_GRANTS.get(config.AUTH_GRANTS.idFromName(name));
    const call = (signal: AbortSignal) => coordinator.fetch("https://grant.internal/verify-scaffold", { method: "POST", body: JSON.stringify({ token: "sc_rc_cancel" }), signal });
    const one = call(first.signal), two = call(second.signal);
    await ready;
    first.abort();
    expect(await (await one).json()).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(authoritySignal.aborted).toBe(false);
    second.abort();
    expect(await (await two).json()).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(authoritySignal.aborted).toBe(true);
    release(new Response(null, { status: 401 }));
    await vi.waitFor(async () => {
      const recovered = await Promise.all(Array.from({ length: 32 }, () => scaffoldAdmission("coordinator", undefined, async () => ({ status: "invalid" }))));
      expect(recovered).toEqual(Array.from({ length: 32 }, () => ({ status: "invalid" })));
    });
    expect(fetcher).toHaveBeenCalledOnce();
  });

  it("admits exactly 32 coordinator waiters and rejects another DO until completion", async () => {
    let started!: () => void, release!: (response: Response) => void;
    const ready = new Promise<void>(resolve => { started = resolve; });
    const fetcher = vi.fn(() => {
      started();
      return new Promise<Response>(resolve => { release = resolve; });
    });
    vi.stubGlobal("fetch", fetcher);
    const token = "sc_rc_waiter_limit";
    const name = await scaffoldVerificationName(config, token);
    const coordinator = config.AUTH_GRANTS.get(config.AUTH_GRANTS.idFromName(name));
    const pending = Array.from({ length: 32 }, () => coordinator.fetch("https://grant.internal/verify-scaffold", {
      method: "POST", body: JSON.stringify({ token })
    }));
    await ready;
    const other = config.AUTH_GRANTS.get(config.AUTH_GRANTS.idFromName(await scaffoldVerificationName(config, "sc_rc_other_limit")));
    expect(await (await other.fetch("https://grant.internal/verify-scaffold", {
      method: "POST", body: JSON.stringify({ token: "sc_rc_other_limit" })
    })).json()).toEqual({ status: "unavailable", retryAfter: "1" });
    expect(fetcher).toHaveBeenCalledOnce();
    release(new Response(null, { status: 401 }));
    expect(await Promise.all(pending.map(async response => (await response).json()))).toEqual(Array.from({ length: 32 }, () => ({ status: "invalid" })));
    fetcher.mockImplementation(async () => new Response(null, { status: 403 }));
    expect(await (await coordinator.fetch("https://grant.internal/verify-scaffold", {
      method: "POST", body: JSON.stringify({ token })
    })).json()).toEqual({ status: "invalid" });
    expect(fetcher).toHaveBeenCalledTimes(2);
  });

  it("rejects authority identity expansion before publishing coordinator JSON", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({
      ok: true, actor: { sub: "x".repeat(40_000), auth: "iap" },
      resource: config.SCAFFOLD_CONTROL_PLANE_URL, scopes: ["remote_code:read"]
    })));
    const token = "sc_rc_expansion";
    const coordinator = config.AUTH_GRANTS.get(config.AUTH_GRANTS.idFromName(await scaffoldVerificationName(config, token)));
    const response = await coordinator.fetch("https://grant.internal/verify-scaffold", { method: "POST", body: JSON.stringify({ token }) });
    expect(await response.json()).toEqual({ status: "unavailable", retryAfter: "1" });
  });
});
