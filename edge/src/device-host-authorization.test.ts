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
        "comet-scaffold-sandbox-1",
        "engine@ashler-local",
        "sandbox"
      ),
      env
    );

    expect(response.status).toBe(403);
    expect(forwardedRequests).toHaveLength(0);
  });
});
