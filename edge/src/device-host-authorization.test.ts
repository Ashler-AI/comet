import { afterEach, describe, expect, it, vi } from "vitest";
import worker from "./index";
import {
  AUTH_GRANT_HEADER,
  AUTH_USER_HEADER,
  DEVICE_HOST_AUTH_HEADER,
  type Env
} from "./env";

const forwardedRequests: Request[] = [];

const deviceRooms = {
  idFromName: (name: string) => name,
  get: (_id: string) => ({
    fetch: async (request: Request) => {
      forwardedRequests.push(request);
      return new Response(null, { status: 204 });
    }
  })
} as unknown as Env["DEVICE_ROOMS"];

const edgeEnv = (authMode: "scaffold" | "dev", environment: "staging" | "local",
  requiredCapabilities = "session.read session.environment"): Env =>
  ({
    AUTH_MODE: authMode,
    ENVIRONMENT: environment,
    SCAFFOLD_CONTROL_PLANE_URL:
      environment === "local"
        ? "http://127.0.0.1:8788"
        : "https://scaffold-staging.internal.ashler.com",
    SCAFFOLD_PROJECT_SCOPE: environment === "local" ? "ashler-local" : "ashler-staging",
    SCAFFOLD_REQUIRED_CAPABILITIES: requiredCapabilities,
    DEVICE_ROOMS: deviceRooms,
    SESSION_ROOMS: {} as Env["SESSION_ROOMS"],
    AUTH_GRANTS: {} as Env["AUTH_GRANTS"],
    BLOBS: {} as R2Bucket
  }) as unknown as Env;

const hostRequest = (
  origin: string,
  deviceId: string,
  token: string,
  spoofedAuthorization?: "local" | "sandbox"
): Request => {
  const headers = new Headers({
    authorization: `Bearer ${token}`,
    upgrade: "websocket"
  });
  if (spoofedAuthorization) {
    headers.set(DEVICE_HOST_AUTH_HEADER, spoofedAuthorization);
  }
  return new Request(`${origin}/device/${deviceId}/ws?role=host&connId=engine&syncProtocol=durable-records-v1`, {
    headers
  });
};

afterEach(() => {
  forwardedRequests.length = 0;
  vi.unstubAllGlobals();
});

describe("trusted device host forwarding", () => {
  it.each([
    "controlDeploymentId=deployment-a", "deploymentId=deployment-a",
    "controlDeploymentId=", "deploymentId="
  ])("rejects ordinary control scope %s instead of forwarding a legacy command", async (scopeQuery) => {
    const env = edgeEnv("dev", "local", "session.read session.chat session.control session.environment");
    const response = await worker.fetch(new Request(
      `http://127.0.0.1/device/local-engine/ws?role=client&purpose=control&controlSessionId=11111111-1111-4111-8111-111111111111&${scopeQuery}`,
      { headers: { authorization: "Bearer engine@ashler-local", upgrade: "websocket" } }
    ), env);
    expect(response.status).toBe(403);
    expect(await response.json()).toEqual({ error: "scoped_control_not_supported" });
    expect(forwardedRequests).toEqual([]);
  });

  it("admits read-only outcome authority only on an exact one-shot session socket", async () => {
    const env = edgeEnv("dev", "local", "session.read");
    for (const [query, status] of [
      ["role=client&purpose=control&controlSessionId=11111111-1111-4111-8111-111111111111", 204],
      ["role=client", 403], ["role=host", 403],
      ["role=client&purpose=control&controlSessionId=invalid", 403]
    ] as const) {
      const response = await worker.fetch(new Request(
        `http://127.0.0.1/device/local-engine/ws?${query}`,
        { headers: { authorization: "Bearer reader@ashler-local", upgrade: "websocket" } }
      ), env);
      expect(response.status).toBe(status);
    }
  });

  it("lets an ordinary local dev engine host and replaces spoofed authority", async () => {
    const env = edgeEnv("dev", "local");
    const response = await worker.fetch(
      hostRequest(
        "http://127.0.0.1",
        "local-engine",
        "engine@ashler-local",
        "sandbox"
      ),
      env
    );

    expect(response.status).toBe(204);
    expect(forwardedRequests).toHaveLength(1);
    const forwarded = forwardedRequests[0]!;
    expect(forwarded.headers.get(DEVICE_HOST_AUTH_HEADER)).toBe("local");
    expect(forwarded.headers.get(AUTH_USER_HEADER)).toBe("engine");
    expect(forwarded.headers.get(AUTH_GRANT_HEADER)).toBeNull();
    expect(forwarded.headers.get("authorization")).toBeNull();
  });

  it("lets a verified OAuth engine with environment authority host locally", async () => {
    const env = edgeEnv("scaffold", "staging");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          ok: true,
          resource: "https://scaffold-staging.internal.ashler.com",
          actor: { sub: "engine@example.com", auth: "iap" },
          scopes: ["remote_code:read", "remote_code:exec"]
        })
      )
    );

    const response = await worker.fetch(
      hostRequest("https://comet.example", "installed-engine", "sc_rc_oauth-engine"),
      env
    );

    expect(response.status).toBe(204);
    expect(forwardedRequests).toHaveLength(1);
    expect(forwardedRequests[0]!.headers.get(DEVICE_HOST_AUTH_HEADER)).toBe("local");
  });

  it("does not let a local credential spoof a sandbox host", async () => {
    const env = edgeEnv("dev", "local");
    const response = await worker.fetch(
      hostRequest(
        "http://127.0.0.1",
        "comet-scaffold-sandbox-1-e1",
        "engine@ashler-local",
        "sandbox"
      ),
      env
    );

    expect(response.status).toBe(403);
    expect(forwardedRequests).toHaveLength(0);
  });

  it("lets a sandbox peer client reach only a session owned by its principal", async () => {
    const grant = {
      userId: "owner@example.com",
      email: "owner@example.com",
      grantId: "a".repeat(32),
      projectId: "ashler-staging",
      deploymentId: "source-deployment",
      sandboxId: "source-sandbox",
      targetDeviceId: "comet-scaffold-source-sandbox-e1",
      sessionId: "11111111-1111-4111-8111-111111111111",
      lifecycleEpoch: 1,
      capabilities: ["session.read", "session.chat"],
      grantedAt: Date.now() - 1,
      expiresAt: Date.now() + 60_000,
      revokedAt: null
    };
    let ownsSession = true;
    let scopedAvailable = false;
    const env = {
      ...edgeEnv("scaffold", "staging"),
      SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat",
      AUTH_GRANTS: {
        idFromName: (id: string) => id,
        get: () => ({ fetch: async () => Response.json(grant) })
      },
      SESSION_ROOMS: {
        idFromName: (id: string) => id,
        get: (room: string) => ({ fetch: async () => Response.json({
          ownsSession: ownsSession && (!room.includes(grant.deploymentId) || scopedAvailable),
          deviceId: "local-engine"
        }) })
      }
    } as unknown as Env;
    const targetSession = "22222222-2222-4222-8222-222222222222";
    const scopedRequest = () => worker.fetch(new Request(
      `https://comet.example/peer/${targetSession}/ws?deploymentId=${grant.deploymentId}&syncProtocol=durable-records-v1`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);
    const directRequest = () => worker.fetch(new Request(
      `https://comet.example/device/local-engine/ws?role=client&purpose=peer&peerSessionId=${targetSession}&syncProtocol=durable-records-v1`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);
    const resolvedRequest = () => worker.fetch(new Request(
      `https://comet.example/peer/${targetSession}/ws?syncProtocol=durable-records-v1`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);

    expect((await directRequest()).status).toBe(204);
    expect((await resolvedRequest()).status).toBe(204);
    expect(new URL(forwardedRequests[1]!.url).searchParams.get("targetDeviceId")).toBe("local-engine");
    expect((await scopedRequest()).status).toBe(404);
    expect(forwardedRequests).toHaveLength(2);
    scopedAvailable = true;
    expect((await scopedRequest()).status).toBe(204);
    ownsSession = false;
    expect((await directRequest()).status).toBe(403);
    expect((await resolvedRequest()).status).toBe(404);
    expect((await scopedRequest()).status).toBe(404);
    expect(forwardedRequests).toHaveLength(3);
  });
});
