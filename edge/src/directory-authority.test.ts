import { afterEach, expect, it, vi } from "vitest";
import worker from "./index";
import { AUTH_USER_HEADER, type Env } from "./env";
import { requestAuthority } from "./authority";

const session = "01956000-0000-7000-8000-000000000001";
const env = {
  AUTH_MODE: "scaffold", ENVIRONMENT: "crew-upstream-canary",
  SCAFFOLD_PROJECT_SCOPE: "canary", SCAFFOLD_DEPLOYMENT_ID: "canary",
  SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.example",
  SCAFFOLD_REQUIRED_CAPABILITIES: "session.read",
} as Env;
afterEach(() => vi.unstubAllGlobals());

it("reads existing chat2 ownership, strips forged claims, and fails closed", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({
    ok: true, actor: { sub: "owner@example.com", auth: "iap" }, resource: "https://scaffold.example",
    scopes: ["remote_code:read", "remote_code:exec"],
  })));
  const roomFetch = vi.fn(async (request: Request) => {
    expect(new URL(request.url).pathname).toBe("/directory-authority");
    expect(request.headers.get(AUTH_USER_HEADER)).toBe("owner@example.com");
    expect(requestAuthority(request)?.projectId).toBe("canary");
    return Response.json({ ownsSession: true });
  });
  const idFromName = vi.fn((name: string) => name);
  const config = { ...env, CHAT_ROOMS: { idFromName, get: () => ({ fetch: roomFetch }) } } as unknown as Env;
  const request = (suffix = "?deploymentId=canary", method = "GET") => new Request(
    `https://edge.example/session/${session}/directory-authority${suffix}`, {
      method, headers: { authorization: "Bearer sc_rc_test", [AUTH_USER_HEADER]: "attacker", "x-crew-authority": "forged" },
    });
  expect(await (await worker.fetch(request(), config)).json()).toEqual({
    ownsSession: true, projectId: "canary", actorId: "owner@example.com", subject: "owner@example.com",
  });
  expect(idFromName).toHaveBeenCalledWith(`chat2/${session}`);
  for (const suffix of ["", "?deploymentId=other", "?deploymentId=canary&projectId=other"]) {
    expect((await worker.fetch(request(suffix), config)).status).toBe(403);
  }
  expect((await worker.fetch(request("?deploymentId=canary", "POST"), config)).status).toBe(405);
  expect(roomFetch).toHaveBeenCalledTimes(1);
  roomFetch.mockImplementationOnce(async () => Response.json({ ownsSession: false }));
  expect((await (await worker.fetch(request(), config)).json() as { ownsSession: boolean }).ownsSession).toBe(false);
  roomFetch.mockImplementationOnce(async () => { throw new Error("unavailable"); });
  expect((await worker.fetch(request(), config)).status).toBe(503);
  roomFetch.mockImplementationOnce(async () => Response.json({ ownsSession: "true" }));
  expect((await worker.fetch(request(), config)).status).toBe(503);
  expect((await worker.fetch(new Request(`https://edge.example/session/${session}/ws`, {
    headers: { authorization: "Bearer sc_rc_test" },
  }), config)).status).toBe(410);
});
