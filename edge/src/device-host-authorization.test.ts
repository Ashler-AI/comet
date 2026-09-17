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

const edgeEnv = (authMode: "scaffold" | "dev", environment: "staging" | "local"): Env =>
  ({
    AUTH_MODE: authMode,
    ENVIRONMENT: environment,
    SCAFFOLD_CONTROL_PLANE_URL:
      environment === "local"
        ? "http://127.0.0.1:8788"
        : "https://scaffold-staging.internal.ashler.com",
    SCAFFOLD_PROJECT_SCOPE: environment === "local" ? "ashler-local" : "ashler-staging",
    SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.environment",
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
  return new Request(`${origin}/device/${deviceId}/ws?role=host&connId=engine`, {
    headers
  });
};

afterEach(() => {
  forwardedRequests.length = 0;
  vi.unstubAllGlobals();
});

describe("trusted device host forwarding", () => {
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
          ownsSession: ownsSession && !room.includes(grant.deploymentId),
          deviceId: "local-engine"
        }) })
      }
    } as unknown as Env;
    const targetSession = "22222222-2222-4222-8222-222222222222";
    const fallbackResolvedRequest = () => worker.fetch(new Request(
      `https://comet.example/peer/${targetSession}/ws?deploymentId=${grant.deploymentId}`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);
    const directRequest = () => worker.fetch(new Request(
      `https://comet.example/device/local-engine/ws?role=client&purpose=peer&peerSessionId=${targetSession}`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);
    const resolvedRequest = () => worker.fetch(new Request(
      `https://comet.example/peer/${targetSession}/ws`,
      { headers: { authorization: `Bearer cs1.${grant.grantId}.${"b".repeat(64)}`, upgrade: "websocket" } }
    ), env);

    expect((await directRequest()).status).toBe(204);
    expect((await resolvedRequest()).status).toBe(204);
    expect(new URL(forwardedRequests[1]!.url).searchParams.get("targetDeviceId")).toBe("local-engine");
    expect((await fallbackResolvedRequest()).status).toBe(204);
    expect(new URL(forwardedRequests[2]!.url).searchParams.get("targetDeviceId")).toBe("local-engine");
    ownsSession = false;
    expect((await directRequest()).status).toBe(403);
    expect((await resolvedRequest()).status).toBe(404);
    expect(forwardedRequests).toHaveLength(3);
  });
});
