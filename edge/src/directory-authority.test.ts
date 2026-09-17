import { afterEach, describe, expect, it, vi } from "vitest";
import worker from "./index";
import { AUTH_PROJECT_HEADER, AUTH_USER_HEADER, SESSION_OWNER_AUTH_HEADER, type Env } from "./env";
import type * as AuthModule from "./auth";
import type * as AuthRoutesModule from "./auth-routes";

const sessionId = "01956000-0000-7000-8000-000000000001";
const identity = { userId: "owner@ashler.ai", email: "owner@ashler.ai", projectScope: "ashler-staging", capabilities: ["session.read", "session.control"], credential: "scaffold" };
vi.mock("./auth", async (importOriginal) => ({
  ...await importOriginal<typeof AuthModule>(),
  authenticateScaffold: vi.fn(async () => identity),
}));
vi.mock("./auth-routes", async (importOriginal) => ({
  ...await importOriginal<typeof AuthRoutesModule>(),
  authenticateDeviceToken: vi.fn(async () => ({ ...identity, credential: "device", projectId: "ashler-staging", deploymentId: "candidate", sessionId })),
}));

afterEach(() => vi.restoreAllMocks());

describe("directory write authority", () => {
  it("checks the actual scoped room owner, replacing forged internal headers", async () => {
    const idFromName = vi.fn((name: string) => name);
    const fetch = vi.fn(async (request: Request) => {
      expect(new URL(request.url).pathname).toBe("/authorize-owner");
      expect(request.headers.get(AUTH_USER_HEADER)).toBe("owner@ashler.ai");
      expect(request.headers.get(AUTH_PROJECT_HEADER)).toBe("ashler-staging");
      expect(request.headers.get(SESSION_OWNER_AUTH_HEADER)).toBe("verify");
      return Response.json({ ownsSession: true });
    });
    const env = { SCAFFOLD_PROJECT_SCOPE: "ashler-staging", SESSION_ROOMS: { idFromName, get: () => ({ fetch }) } } as unknown as Env;
    const response = await worker.fetch(new Request(`https://crew.test/session/${sessionId}/directory-authority?deploymentId=candidate`, {
      headers: { authorization: "Bearer sc_rc_user", [AUTH_USER_HEADER]: "attacker", [AUTH_PROJECT_HEADER]: "other" },
    }), env);
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ownsSession: true, projectId: "ashler-staging", actorId: identity.email, subject: identity.userId });
    expect(idFromName).toHaveBeenCalledWith(`s4/ashler-staging/candidate/${sessionId}`);
  });

  it("allows only the exact device session and deployment", async () => {
    const get = vi.fn(() => ({ fetch: async () => Response.json({ ownsSession: true }) }));
    const env = { SCAFFOLD_PROJECT_SCOPE: "ashler-staging", SESSION_ROOMS: { idFromName: (name: string) => name, get } } as unknown as Env;
    for (const path of [`${sessionId}/directory-authority`, `${sessionId}/directory-authority?deploymentId=other`, "01956000-0000-7000-8000-000000000002/directory-authority?deploymentId=candidate"]) {
      const response = await worker.fetch(new Request(`https://crew.test/session/${path}`, {
        headers: { authorization: "Bearer cs1.scoped.device" },
      }), env);
      expect(response.status).toBe(403);
    }
    expect(get).not.toHaveBeenCalled();
    const response = await worker.fetch(new Request(`https://crew.test/session/${sessionId}/directory-authority?deploymentId=candidate`, {
      headers: { authorization: "Bearer cs1.scoped.device" },
    }), env);
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ ownsSession: true, actorId: identity.email });
  });

  it("returns nonownership without allowing first-write claims and makes outages retryable", async () => {
    const fetch = vi.fn(async () => Response.json({ ownsSession: false }));
    const env = { SCAFFOLD_PROJECT_SCOPE: "ashler-staging", SESSION_ROOMS: { idFromName: (name: string) => name, get: () => ({ fetch }) } } as unknown as Env;
    const request = new Request(`https://crew.test/session/${sessionId}/directory-authority`, { headers: { authorization: "Bearer sc_rc_user" } });
    expect(await (await worker.fetch(request, env)).json()).toEqual({ ownsSession: false, projectId: "ashler-staging", actorId: identity.email, subject: identity.userId });
    fetch.mockRejectedValueOnce(new Error("unavailable"));
    expect((await worker.fetch(request, env)).status).toBe(503);
  });
});
