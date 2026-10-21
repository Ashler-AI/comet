import { afterEach, expect, it, vi } from "vitest";
import worker from "./index";
import type { Env } from "./env";
import { authorizeRoute } from "./authority";
import { fetchReleaseObject } from "./release-feed";

vi.mock("./release-feed", () => ({ fetchReleaseObject: vi.fn(async () => new Response("private artifact")) }));
const env = {
  AUTH_MODE: "scaffold", ENVIRONMENT: "staging",
  SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold-staging.internal.ashler.com",
  SCAFFOLD_PROJECT_SCOPE: "ashler-staging", SCAFFOLD_DEPLOYMENT_ID: "ashler-staging",
  SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat session.control session.annotate session.invite session.files session.environment",
} as Env;
const authority = (resource = env.SCAFFOLD_CONTROL_PLANE_URL, scopes = ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec"]) =>
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({
    ok: true, actor: { sub: "owner@example.com", auth: "iap" }, resource, scopes,
  })));
const request = (path: string, method = "GET", token = "sc_rc_existing-stage-client") =>
  new Request(`https://comet-staging.internal.ashler.com${path}`, {
    method, headers: token ? { authorization: `Bearer ${token}` } : {},
  });
afterEach(() => { vi.unstubAllGlobals(); vi.clearAllMocks(); });

it("keeps existing staging bearer manifest, archive, signature and installer access behind authentication", async () => {
  authority();
  for (const [path, file, method] of [
    ["/api/releases/desktop-staging-manifest.json", "desktop-staging-manifest.json", "GET"],
    ["/api/releases/crew-staging-aarch64-apple-darwin.tar.gz", "crew-staging-aarch64-apple-darwin.tar.gz", "HEAD"],
    ["/releases/crew-staging-aarch64-apple-darwin.tar.gz.sig", "crew-staging-aarch64-apple-darwin.tar.gz.sig", "GET"],
    ["/install.sh", "install.sh", "GET"],
  ]) {
    expect((await worker.fetch(request(path, method), env)).status).toBe(200);
    expect(fetchReleaseObject).toHaveBeenLastCalledWith(env, file, method);
  }
  vi.mocked(fetchReleaseObject).mockClear();
  expect((await worker.fetch(request("/api/releases/desktop-staging-manifest.json", "GET", ""), env)).status).toBe(401);
  expect(fetchReleaseObject).not.toHaveBeenCalled();
});

it("fails closed for other authority resources, insufficient scopes, unavailable authority and cross-scope feed requests", async () => {
  for (const resource of ["https://scaffold.internal.ashler.com", "https://scaffold-upstream-canary.internal.ashler.com"]) {
    authority(resource);
    expect((await worker.fetch(request("/api/releases/desktop-staging-manifest.json"), env)).status).toBe(503);
  }
  authority(undefined, ["remote_code:read"]);
  expect((await worker.fetch(request("/api/releases/desktop-staging-manifest.json"), env)).status).toBe(503);
  vi.stubGlobal("fetch", vi.fn(async () => { throw new Error("offline"); }));
  expect((await worker.fetch(request("/api/releases/desktop-staging-manifest.json"), env)).status).toBe(503);
  authority();
  for (const path of ["/api/releases/desktop-staging-manifest.json?projectId=ashler-production", "/install.sh?deploymentId=production"]) {
    expect((await worker.fetch(request(path), env)).status).toBe(403);
  }
  expect(fetchReleaseObject).not.toHaveBeenCalled();
});

it("rejects release writes and malformed filenames and never opens obsolete workspace/session lanes", async () => {
  authority();
  for (const path of ["/api/releases/manifest.json", "/releases/manifest.json", "/install.sh"]) {
    expect((await worker.fetch(request(path, "POST"), env)).status).toBe(403);
  }
  for (const path of ["/api/releases/a%2Fb", "/api/releases/a%ZZ", "/releases/a..b", "/api/releases/file/extra"]) {
    expect((await worker.fetch(request(path), env)).status).toBe(400);
  }
  for (const path of ["/workspace/ashler-staging/ws", "/session/old/ws", "/session/old/snapshot", "/append", "/snapshot"]) {
    expect((await worker.fetch(request(path, "POST"), env)).status).toBe(410);
  }
  expect(fetchReleaseObject).not.toHaveBeenCalled();
});

it("never acknowledges discarded legacy attachments or touches native blob storage", async () => {
  authority();
  const put = vi.fn();
  const get = vi.fn();
  const config = { ...env, BLOBS: { put, get } } as unknown as Env;
  for (const method of ["PUT", "POST", "GET", "HEAD", "DELETE"]) {
    const response = await worker.fetch(request(`/attachments/${"a".repeat(64)}`, method), config);
    expect(response.status).toBe(410);
    expect(await response.json()).toEqual({ error: "legacy_attachments_retired" });
  }
  expect(put).not.toHaveBeenCalled();
  expect(get).not.toHaveBeenCalled();
  expect((await worker.fetch(request("/attachments/old", "PUT", ""), config)).status).toBe(401);
});

it("allows read-scoped device release access without granting legacy workspace access", () => {
  const identity = {
    userId: "owner", email: "owner@example.com", projectScope: "ashler-staging", deploymentId: "ashler-staging",
    credential: "device" as const, capabilities: ["session.read"], sessionId: "session", targetDeviceId: "device",
  };
  for (const path of ["/install.sh", "/api/releases/desktop-staging-manifest.json"]) {
    expect(authorizeRoute(request(path), identity)).toBe(true);
    expect(authorizeRoute(request(path, "POST"), identity)).toBe(false);
    expect(authorizeRoute(request(path), { ...identity, capabilities: [] })).toBe(false);
  }
  expect(authorizeRoute(request("/workspace/ashler-staging/ws"), identity)).toBe(false);
});
