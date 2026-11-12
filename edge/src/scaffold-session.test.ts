import { afterEach, beforeEach, describe, expect, it, vi, type MockInstance } from "vitest";
import { authenticateResult, type ScaffoldAuthenticationResult } from "./auth";
import { AuthGrant, isolateScaffoldSessionCount, resetIsolateScaffoldSessions, scaffoldVerificationName, verifyScaffoldThroughCoordinator } from "./auth-routes";
import { requestAuthority, socketAuthorized, trustedRequest } from "./authority";
import type { Env } from "./env";
import {
  DEFAULT_SESSION_FRESH_MS, DEFAULT_SESSION_GRACE_MS, ISOLATE_RECHECK_MS, ISOLATE_SESSION_LIMIT,
  SessionCache, sessionPolicy, verifiedSession
} from "./scaffold-session";

const ORIGIN = "https://scaffold.example";
const baseConfig = {
  AUTH_MODE: "scaffold", ENVIRONMENT: "crew-staging", SCAFFOLD_PROJECT_SCOPE: "ashler", SCAFFOLD_DEPLOYMENT_ID: "staging",
  SCAFFOLD_CONTROL_PLANE_URL: ORIGIN, SCAFFOLD_REQUIRED_CAPABILITIES: "session.read"
};
const FRESH = DEFAULT_SESSION_FRESH_MS;
const GRACE = DEFAULT_SESSION_GRACE_MS;

/** Scaffold `/auth/session`, per bearer. `hang` holds replies until released. */
interface Upstream { status: number; scopes: string[]; hang: boolean }
let upstream: Map<string, Upstream>;
let held: Array<() => void>;
const scaffold = vi.fn(async (url: string, init: RequestInit) => {
  const token = new Headers(init.headers).get("authorization")!.slice("Bearer ".length);
  const state = upstream.get(token);
  if (!state) throw new Error("unexpected authority token");
  if (state.hang) {
    await new Promise<void>((resolve, reject) => {
      held.push(resolve);
      init.signal?.addEventListener("abort", () => { reject(new Error("aborted")); }, { once: true });
    });
  }
  if (state.status !== 200) return new Response("", { status: state.status });
  return Response.json({ ok: true, resource: new URL(url).origin, actor: { sub: `${token.slice(6)}@example.com`, auth: "iap" }, scopes: state.scopes });
});
const token = (user: string, scopes = ["remote_code:read"]) => {
  upstream.set(`sc_rc_${user}`, { status: 200, scopes, hang: false });
  return `sc_rc_${user}`;
};
const set = (bearer: string, change: Partial<Upstream>) => { Object.assign(upstream.get(bearer)!, change); };
const scaffoldCalls = () => scaffold.mock.calls.length;

/** Coordinators share durable storage across restarts; memory does not survive one. */
const coordinators = (overrides: Partial<Env> = {}) => {
  const env = { ...baseConfig, ...overrides } as Env;
  const stores = new Map<string, Map<string, unknown>>();
  const alarms = new Map<string, number | null>();
  const live = new Map<string, AuthGrant>();
  let hops = 0;
  const instance = (id: string) => {
    let grant = live.get(id);
    if (grant) return grant;
    const values = stores.get(id) ?? new Map<string, unknown>();
    stores.set(id, values);
    grant = new AuthGrant({
      id,
      storage: {
        get: async (key: string) => structuredClone(values.get(key)),
        put: async (key: string, value: unknown) => { values.set(key, structuredClone(value)); },
        delete: async (key: string) => values.delete(key),
        setAlarm: async (at: number) => { alarms.set(id, at); },
        deleteAlarm: async () => { alarms.set(id, null); }
      },
      blockConcurrencyWhile: () => { throw new Error("verification must not enter grant storage"); }
    } as unknown as DurableObjectState, env);
    live.set(id, grant);
    return grant;
  };
  env.AUTH_GRANTS = {
    idFromName: (name: string) => name,
    get: (id: string) => ({
      fetch: (input: string | Request, init?: RequestInit) => {
        hops++;
        return instance(id).fetch(input instanceof Request ? input : new Request(input, init));
      }
    })
  } as unknown as DurableObjectNamespace;
  return {
    env,
    hops: () => hops,
    restart: () => { live.clear(); },
    record: async (bearer: string) => stores.get(await scaffoldVerificationName(env, bearer))?.get("scaffold-session"),
    alarmAt: async (bearer: string) => alarms.get(await scaffoldVerificationName(env, bearer)),
    fireAlarm: async (bearer: string) => instance(await scaffoldVerificationName(env, bearer)).alarm()
  };
};

const request = (bearer: string) => new Request("https://edge.example/registry/ashler/rows", { headers: { authorization: `Bearer ${bearer}` } });
const check = (env: Env, bearer: string): Promise<ScaffoldAuthenticationResult> => authenticateResult(env, request(bearer));
const socketFor = async (env: Env, bearer: string) => {
  const result = await check(env, bearer);
  if (result.status !== "authenticated") throw new Error("expected an authenticated socket");
  const authority = requestAuthority(trustedRequest(new Request(`https://edge.example/registry/ashler/ws?token=${bearer}`), result.identity));
  const close = vi.fn();
  return { socket: { deserializeAttachment: () => ({ authority }), close } as unknown as WebSocket, close };
};

let start: number;
let clock: MockInstance<typeof Date.now>;
const at = (offset: number) => { clock.mockReturnValue(start + offset); };

beforeEach(() => {
  upstream = new Map();
  held = [];
  scaffold.mockClear();
  vi.stubGlobal("fetch", scaffold);
  resetIsolateScaffoldSessions();
  start = Date.now();
  clock = vi.spyOn(Date, "now").mockReturnValue(start);
});
afterEach(async () => {
  for (const state of upstream.values()) Object.assign(state, { hang: false, status: 503 });
  for (const release of held.splice(0)) release();
  // Let detached refreshes settle against this test's authority, never the next one's.
  await new Promise(resolve => setTimeout(resolve, 20));
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("Scaffold slow or unavailable", () => {
  it("reuses a fresh verification with no Scaffold call and no coordinator hop", async () => {
    const { env, hops } = coordinators();
    const bearer = token("owner");
    for (let n = 0; n < 50; n++) expect((await check(env, bearer)).status).toBe("authenticated");
    expect(scaffoldCalls()).toBe(1);
    expect(hops()).toBe(1);
    at(FRESH - 1);
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(scaffoldCalls()).toBe(1);
  });

  it("keeps a recently verified session (HTTP and sockets) working through an outage, until the grace bound", async () => {
    const { env } = coordinators();
    const bearer = token("owner");
    const { socket, close } = await socketFor(env, bearer);
    set(bearer, { status: 503 });
    for (const offset of [FRESH + 1, FRESH + 60 * 60_000, GRACE - 1]) {
      at(offset);
      resetIsolateScaffoldSessions(); // every request on a cold isolate: the coordinator record answers
      expect((await check(env, bearer)).status, `t+${offset}`).toBe("authenticated");
      expect(await socketAuthorized(env, socket)).toBe(true);
    }
    expect(close).not.toHaveBeenCalled();
    at(GRACE);
    expect(await check(env, bearer)).toMatchObject({ status: "unavailable" });
    expect(await socketAuthorized(env, socket)).toBe(false);
    expect(close).toHaveBeenCalledWith(1013, "authority unavailable");
  });

  it("answers from the stale record within about a second while Scaffold hangs, with one upstream call", async () => {
    const { env, hops } = coordinators();
    const bearer = token("owner");
    await check(env, bearer);
    set(bearer, { hang: true });
    at(FRESH + 1);
    const began = performance.now();
    const results = await Promise.all(Array.from({ length: 20 }, () => check(env, bearer)));
    expect(performance.now() - began).toBeLessThan(1_900);
    expect(results.every(result => result.status === "authenticated")).toBe(true);
    // One shared consultation per isolate and one Scaffold refresh in flight.
    expect(hops()).toBe(2);
    expect(scaffoldCalls()).toBe(2);
    // The isolate then reuses without asking again until its recheck delay passes.
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(hops()).toBe(2);
    at(FRESH + 1 + ISOLATE_RECHECK_MS);
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(hops()).toBe(3);
    expect(scaffoldCalls()).toBe(2);
  }, 10_000);

  it("lets a slow refresh outlive its callers and renew the record", async () => {
    const { env, record } = coordinators();
    const bearer = token("owner");
    await check(env, bearer);
    set(bearer, { hang: true });
    at(FRESH + 1);
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(await record(bearer)).toMatchObject({ verifiedAt: start });
    set(bearer, { hang: false });
    for (const release of held.splice(0)) release();
    await vi.waitFor(async () => { expect(await record(bearer)).toMatchObject({ verifiedAt: start + FRESH + 1 }); });
    resetIsolateScaffoldSessions();
    const calls = scaffoldCalls();
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(scaffoldCalls()).toBe(calls);
  }, 10_000);

  it("never serves a session that crosses its grace bound while the authority is consulted", async () => {
    const { env } = coordinators();
    const bearer = token("owner");
    await check(env, bearer);
    set(bearer, { hang: true });
    at(GRACE - 10);
    const pending = check(env, bearer);
    await new Promise(resolve => setTimeout(resolve, 50));
    at(GRACE + 1);
    expect(await pending).toMatchObject({ status: "unavailable" });
  }, 10_000);

  it("carries the record across a coordinator restart and drops it at the grace bound", async () => {
    const { env, restart, record, alarmAt, fireAlarm } = coordinators();
    const bearer = token("owner");
    await check(env, bearer);
    expect(await alarmAt(bearer)).toBe(start + GRACE);
    set(bearer, { status: 503 });
    at(FRESH + 1);
    restart();
    resetIsolateScaffoldSessions();
    expect((await check(env, bearer)).status).toBe("authenticated");
    at(GRACE);
    await fireAlarm(bearer);
    expect(await record(bearer)).toBeUndefined();
    restart();
    resetIsolateScaffoldSessions();
    expect(await check(env, bearer)).toMatchObject({ status: "unavailable" });
  });

  it("recovers to fresh reuse as soon as Scaffold answers again", async () => {
    const { env } = coordinators();
    const bearer = token("owner");
    await check(env, bearer);
    set(bearer, { status: 503 });
    at(FRESH + 1);
    expect((await check(env, bearer)).status).toBe("authenticated");
    const calls = scaffoldCalls();
    set(bearer, { status: 200 });
    at(FRESH + 1 + ISOLATE_RECHECK_MS + 1_001);
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(scaffoldCalls()).toBe(calls + 1);
    // A new success restarts the fresh window; no further calls within it.
    at(FRESH + 1 + ISOLATE_RECHECK_MS + 1_001 + FRESH - 10);
    expect((await check(env, bearer)).status).toBe("authenticated");
    expect(scaffoldCalls()).toBe(calls + 1);
  });
});

describe("revocation and capability changes", () => {
  it("stops a revoked token once the fresh bound passes and never revives it during a later outage", async () => {
    const { env, record } = coordinators();
    const bearer = token("owner");
    const { socket, close } = await socketFor(env, bearer);
    set(bearer, { status: 401 });
    at(FRESH - 1);
    expect((await check(env, bearer)).status).toBe("authenticated"); // the documented bound
    at(FRESH + 1);
    expect(await check(env, bearer)).toEqual({ status: "invalid" });
    expect(await record(bearer)).toBeUndefined();
    expect(await socketAuthorized(env, socket)).toBe(false);
    expect(close).toHaveBeenCalledWith(4401, "authority expired or revoked");
    set(bearer, { status: 503 });
    at(FRESH + 10_000);
    resetIsolateScaffoldSessions();
    expect(await check(env, bearer)).toMatchObject({ status: "unavailable" });
  });

  it("stops a revoked token in every warm isolate within one fresh period", async () => {
    const { env } = coordinators();
    const bearer = token("owner");
    await check(env, bearer); // isolate A warm
    set(bearer, { status: 403 });
    at(FRESH + 1);
    resetIsolateScaffoldSessions(); // isolate B: cold, learns revocation from Scaffold
    expect(await check(env, bearer)).toEqual({ status: "invalid" });
    // Isolate A's entry is also past fresh, so it consults the coordinator's rejection.
    expect(await check(env, bearer)).toEqual({ status: "invalid" });
  });

  it("applies a narrower capability set at the next refresh and closes sockets bound to the wider one", async () => {
    const { env } = coordinators();
    const bearer = token("owner", ["remote_code:read", "remote_code:write", "remote_code:exec"]);
    const wide = await check(env, bearer);
    expect(wide.status === "authenticated" && wide.identity.capabilities).toContain("session.control");
    const { socket, close } = await socketFor(env, bearer);
    set(bearer, { scopes: ["remote_code:read"] });
    at(FRESH + 1);
    const narrow = await check(env, bearer);
    expect(narrow.status === "authenticated" && narrow.identity.capabilities).toEqual(["session.read"]);
    expect(await socketAuthorized(env, socket)).toBe(false);
    expect(close).toHaveBeenCalledWith(4401, "authority expired or revoked");
  });

  it("treats losing a required capability as definitive: public 503, purged, and never served stale", async () => {
    const { env, record } = coordinators({ SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.control" });
    const bearer = token("owner", ["remote_code:read", "remote_code:exec"]);
    expect((await check(env, bearer)).status).toBe("authenticated");
    set(bearer, { scopes: ["remote_code:read"] });
    at(FRESH + 1);
    expect(await check(env, bearer)).toEqual({ status: "unavailable" });
    expect(await record(bearer)).toBeUndefined();
    set(bearer, { status: 503 });
    at(FRESH + 10_000);
    expect(await check(env, bearer)).toMatchObject({ status: "unavailable" });
    resetIsolateScaffoldSessions();
    expect(await check(env, bearer)).toMatchObject({ status: "unavailable" });
  });
});

describe("tenancy and bounds", () => {
  it("never reuses a verification across tokens, tenants or authority configuration", async () => {
    const owner = token("owner"), other = token("other");
    const { env } = coordinators();
    expect(await check(env, owner)).toMatchObject({ status: "authenticated", identity: { userId: "owner@example.com" } });
    set(other, { status: 503 });
    expect(await check(env, other)).toMatchObject({ status: "unavailable" });
    set(other, { status: 200 });
    at(1_001);
    expect(await check(env, other)).toMatchObject({ status: "authenticated", identity: { userId: "other@example.com" } });
    set(owner, { status: 503 });
    for (const overrides of [
      { SCAFFOLD_PROJECT_SCOPE: "other-project" }, { SCAFFOLD_DEPLOYMENT_ID: "production" },
      { ENVIRONMENT: "production" }, { SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat" },
      { SCAFFOLD_CONTROL_PLANE_URL: "https://other-scaffold.example" }
    ]) {
      // Same bearer, different authority configuration: nothing to reuse, so no stale fallback.
      const changed = { ...env, ...overrides } as Env;
      expect(await check(changed, owner), JSON.stringify(overrides)).toMatchObject({ status: "unavailable" });
    }
    expect(await check(env, owner)).toMatchObject({ status: "authenticated", identity: { userId: "owner@example.com" } });
  });

  it("keys the isolate cache by the full coordinator name and bounds it", async () => {
    const { env } = coordinators();
    for (let user = 0; user < ISOLATE_SESSION_LIMIT + 44; user++) {
      expect((await verifyScaffoldThroughCoordinator(env, token(`user${user}`))).status).toBe("authenticated");
    }
    expect(isolateScaffoldSessionCount()).toBe(ISOLATE_SESSION_LIMIT);
    // The most recent sessions remain; the oldest were evicted and must ask again.
    const calls = scaffoldCalls();
    await verifyScaffoldThroughCoordinator(env, `sc_rc_user${ISOLATE_SESSION_LIMIT + 43}`);
    expect(scaffoldCalls()).toBe(calls);
  });

  it("evicts least recently used entries, drops expired ones and only shortens bounds", () => {
    const policy = { freshMs: 1_000, graceMs: 10_000 };
    const cache = new SessionCache<never>(3);
    const identity = { userId: "owner@example.com", email: "owner@example.com", projectScope: "ashler", orgId: "ashler", capabilities: ["session.read"], credential: "scaffold" as const };
    for (const key of ["a", "b", "c"]) cache.put(key, verifiedSession(identity, 0, policy), 0, policy);
    expect(cache.get("a", 1)).toBeDefined(); // touch: b is now least recent
    cache.put("d", verifiedSession(identity, 0, policy), 0, policy);
    expect(cache.size).toBe(3);
    expect(cache.get("b", 1)).toBeUndefined();
    expect(cache.get("a", 10_000)).toBeUndefined();
    expect(cache.size).toBe(2);
    // A reply claiming wider bounds than local policy is clamped to it.
    cache.put("e", { identity, verifiedAt: 0, freshUntil: 999_999, staleUntil: 9_999_999 }, 0, policy);
    expect(cache.get("e", 0)?.session).toMatchObject({ freshUntil: 1_000, staleUntil: 10_000 });
    // A record verified implausibly far in the future is not reusable.
    cache.put("f", verifiedSession(identity, 120_000, policy), 0, policy);
    expect(cache.get("f", 0)).toBeUndefined();
  });

  it("clamps operator policy to safe bounds", () => {
    expect(sessionPolicy({})).toEqual({ freshMs: FRESH, graceMs: GRACE });
    expect(sessionPolicy({ SCAFFOLD_SESSION_FRESH_MS: "5", SCAFFOLD_SESSION_GRACE_MS: "1" })).toEqual({ freshMs: 100, graceMs: 100 });
    expect(sessionPolicy({ SCAFFOLD_SESSION_FRESH_MS: "99999999", SCAFFOLD_SESSION_GRACE_MS: "999999999" })).toEqual({ freshMs: 3_600_000, graceMs: 604_800_000 });
    expect(sessionPolicy({ SCAFFOLD_SESSION_FRESH_MS: "-1", SCAFFOLD_SESSION_GRACE_MS: "1e9" })).toEqual({ freshMs: FRESH, graceMs: GRACE });
  });

  it("admits but never reuses a positive reply without bounds from a pre-reuse coordinator", async () => {
    const identity = { userId: "owner@example.com", email: "owner@example.com", projectScope: "ashler", orgId: "ashler", capabilities: ["session.read"], credential: "scaffold" };
    const fetcher = vi.fn(async () => Response.json({ status: "authenticated", identity }));
    const env = { ...baseConfig, AUTH_GRANTS: { idFromName: (name: string) => name, get: () => ({ fetch: fetcher }) } } as unknown as Env;
    expect((await verifyScaffoldThroughCoordinator(env, "sc_rc_legacy")).status).toBe("authenticated");
    expect((await verifyScaffoldThroughCoordinator(env, "sc_rc_legacy")).status).toBe("authenticated");
    expect(fetcher).toHaveBeenCalledTimes(2);
    expect(isolateScaffoldSessionCount()).toBe(0);
  });
});
