import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, it, vi } from "vitest";
import * as production from "./production";
import native from "./index";

const { default: worker, activeProductionEnv } = production;
const edgeDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const LEGACY_WORKER = "ashler-comet-edge-production";
const LEGACY_ORIGIN = "comet.internal.ashler.com";

const bucket = () => ({ get: vi.fn(async () => null), head: vi.fn(async () => null), put: vi.fn(async () => null) });
const rawEnv = () => {
  const legacy = vi.fn(async () => new Response("legacy release bytes"));
  const raw = {
    AUTH_MODE: "scaffold", ENVIRONMENT: "production",
    SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.internal.ashler.com",
    SCAFFOLD_PROJECT_SCOPE: "ashler-production", SCAFFOLD_DEPLOYMENT_ID: "ashler-production",
    SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.sync.write session.chat session.control session.annotate session.files session.environment",
    AUTH_GRANTS: { name: "grants" }, DEVICE_ROOMS: {}, PREVIEW_ROOMS: {}, REGISTRY_ROOMS: {}, CHAT_ROOMS: {},
    BLOBS: bucket(), LEGACY_EDGE: { fetch: legacy }, APNS_PRIVATE_KEY: "p8"
  } as unknown as production.ProductionEnv;
  return { raw, legacy };
};
const request = (route: string, method = "GET") => new Request(`https://crew.internal.ashler.com${route}`, {
  method, headers: { authorization: "Bearer sc_rc_production-client" }
});
afterEach(() => { vi.restoreAllMocks(); });

it("hands authenticated release reads to the legacy Worker with the caller's own request", async () => {
  const { raw, legacy } = rawEnv();
  const nativeFetch = vi.spyOn(native, "fetch");
  for (const method of ["GET", "HEAD"]) {
    const read = request("/api/releases/desktop-manifest.json", method);
    expect(await (await worker.fetch(read, raw)).text()).toBe("legacy release bytes");
    expect(legacy).toHaveBeenLastCalledWith(read);
  }
  expect(nativeFetch).not.toHaveBeenCalled();
});

it("opens no other release, installer or write path", async () => {
  const { raw, legacy } = rawEnv();
  const nativeFetch = vi.spyOn(native, "fetch");
  for (const [route, method] of [
    ["/api/releases/desktop-manifest.json", "POST"], ["/api/releases/desktop-manifest.json", "DELETE"],
    ["/api/releases/a%2Fb", "GET"], ["/api/releases/file/extra", "GET"], ["/api/releases", "GET"],
    ["/releases/desktop-manifest.json", "GET"], ["/install.sh", "GET"]
  ]) {
    expect((await worker.fetch(request(route, method), raw)).status).toBe(404);
  }
  expect(legacy).not.toHaveBeenCalled();
  expect(nativeFetch).not.toHaveBeenCalled();
});

it("serves everything else natively and never forwards legacy lanes to the legacy Worker", async () => {
  const { raw, legacy } = rawEnv();
  const health = await worker.fetch(new Request("https://crew.internal.ashler.com/health"), raw);
  expect(await health.json()).toEqual({ ok: true, auth: "scaffold", environment: "production" });
  const nativeFetch = vi.spyOn(native, "fetch").mockResolvedValue(new Response("native", { status: 418 }));
  for (const route of ["/workspace/ashler-production/ws", "/session/old/ws", "/device/comet-scaffold-ses_x-e1/status", "/auth/device-grants"]) {
    const call = request(route);
    expect((await worker.fetch(call, raw)).status).toBe(418);
    const [forwarded, env] = nativeFetch.mock.lastCall!;
    expect(forwarded).toBe(call);
    expect(env).toMatchObject({ ENVIRONMENT: "production", SCAFFOLD_DEPLOYMENT_ID: "ashler-production" });
    expect("LEGACY_EDGE" in env).toBe(false);
  }
  expect(legacy).not.toHaveBeenCalled();
});

it("gives native code a fenced bucket, the epoch fence and no legacy binding", async () => {
  const { raw } = rawEnv();
  const env = activeProductionEnv(raw);
  await env.BLOBS.put("blob/user/chat/part", "bytes");
  await env.BLOBS.get("backup/chat2/id/latest.json");
  await env.BLOBS.head("blob/user/chat/part");
  const blobs = raw.BLOBS as unknown as ReturnType<typeof bucket>;
  expect(blobs.put).toHaveBeenCalledWith("native-v2/blob/user/chat/part", "bytes", undefined);
  expect(blobs.get).toHaveBeenCalledWith("native-v2/backup/chat2/id/latest.json", undefined);
  expect(blobs.head).toHaveBeenCalledWith("native-v2/blob/user/chat/part");
  expect(env.NATIVE_GRANT_EPOCHS).toBe(raw.AUTH_GRANTS);
  expect(env.APNS_KEY_P8).toBe("p8");
  expect("LEGACY_EDGE" in env).toBe(false);
  expect(() => env.SESSION_ROOMS.idFromName("s4/x")).toThrow(/not bound/);
});

it("keeps native production a separate Worker that cannot replace legacy production", async () => {
  const { experimental_readRawConfig } = await import("wrangler");
  const { rawConfig } = experimental_readRawConfig({ config: path.join(edgeDir, "wrangler.jsonc") }, { useRedirectIfAvailable: false });
  const config = rawConfig.env?.production;
  expect(config).toBeDefined();
  // Deploying a native config over the legacy Worker would drop the DO classes legacy sessions use.
  expect(config!.name).not.toBe(LEGACY_WORKER);
  expect(config!.routes?.map((route) => typeof route === "string" ? route : route.pattern)).not.toContain(LEGACY_ORIGIN);
  expect(config!.services).toEqual([{ binding: "LEGACY_EDGE", service: LEGACY_WORKER }]);
  const classes = config!.durable_objects!.bindings.map((binding) => binding.class_name);
  for (const name of classes) {
    expect(name).toMatch(/^Native/);
    expect(typeof (production as Record<string, unknown>)[name]).toBe("function");
  }
  for (const migration of config!.migrations ?? []) {
    expect(migration).not.toHaveProperty("deleted_classes");
    expect(migration).not.toHaveProperty("renamed_classes");
  }
  expect(config!.migrations!.flatMap((migration) => migration.new_sqlite_classes ?? []).sort()).toEqual([...classes].sort());
  expect(config!.vars).toMatchObject({
    ENVIRONMENT: "production", SCAFFOLD_PROJECT_SCOPE: "ashler-production",
    SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.internal.ashler.com"
  });

  // The packaged production client must dial this Worker, not the legacy origin.
  const defaults = await readFile(path.join(edgeDir, "../apps/zeron/src/environment.rs"), "utf8");
  const block = defaults.slice(defaults.indexOf("const PRODUCTION: Defaults"));
  const origin = /edge_url: "https:\/\/([^"]+)"/.exec(block)?.[1];
  const route = config!.routes?.[0];
  expect(origin).toBe(typeof route === "string" ? route : route?.pattern);
});
