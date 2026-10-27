import { createHash, generateKeyPairSync, randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { build } from "esbuild";
import { Miniflare, Response, type WorkerOptions } from "miniflare";
import { afterEach, beforeAll, beforeEach, expect, it } from "vitest";
import { decodeFrame, encodeFrame, FRAME } from "../../src/chat-frames";
import { NATIVE_GRANT_ISSUER_HEADER } from "../../src/native-grant-epoch";
import { decodeDeviceFrame } from "../../src/device-room";

const root = resolve(import.meta.dirname, "../..");
const workerName = "canonical-retention";
const rawScope = "retained-scope"; // Deliberately differs: detect mutation/remapping of raw legacy vars.
const nativeScope = "ashler-staging";
const oldCapabilities = "session.read session.chat session.control session.annotate session.invite session.files session.environment";
const nativeCapabilities = ["session.read", "session.sync.write", "session.chat", "session.control", "session.annotate", "session.files", "session.environment"];
const oldSecret = "a".repeat(64);
const nativeSecret = "b".repeat(64);
const wasmName = "2f0f4e9e57402ed68e2f85508ca0967828e35738-loro_wasm_bg.wasm";
const classes = {
  AUTH_GRANTS: "AuthGrant", DEVICE_ROOMS: "DeviceRoom", SESSION_ROOMS: "SessionRoom",
  NATIVE_AUTH_GRANTS: "NativeAuthGrant", NATIVE_DEVICE_ROOMS: "NativeDeviceRoom",
  NATIVE_PREVIEW_ROOMS: "NativePreviewRoom", NATIVE_REGISTRY_ROOMS: "NativeRegistryRoom",
  NATIVE_CHAT_ROOMS: "NativeChatRoom", TEST_ALARM_SESSION: "RetainedAlarmSession"
};
let activeModules: WorkerOptions["modules"];
let rollbackModules: WorkerOptions["modules"];
let privateKey: string;
let mf: Miniflare;
let outbound: Array<{ url: string; method: string; authorization: string | null; body: string }>;
let authorityVersion: string;
let authorityEpoch: number;
let delayedAuthority: { epoch: number; captured: () => void; release: Promise<void> } | undefined;

async function modulesFor(entry: string): Promise<WorkerOptions["modules"]> {
  const result = await build({
    absWorkingDir: root, entryPoints: [`test/staging/${entry}`],
    outfile: resolve(root, "test/staging/worker.mjs"),
    bundle: true, write: false, format: "esm", platform: "browser", target: "es2022",
    alias: { "loro-crdt": "loro-crdt/base64" },
    external: ["node:*", "cloudflare:*", "*.wasm"]
  });
  return [
    { type: "ESModule", path: resolve(root, "test/staging/worker.mjs"), contents: result.outputFiles[0].text },
    { type: "CompiledWasm", path: resolve(root, "test/staging", wasmName), contents: await readFile(resolve(root, "legacy/ae2", wasmName)) }
  ];
}

function options(modules = activeModules): WorkerOptions {
  return {
    name: workerName, modules, modulesRoot: root,
    compatibilityDate: "2026-07-01", compatibilityFlags: ["nodejs_compat"],
    bindings: {
      AUTH_MODE: "dev", ENVIRONMENT: "local", SCAFFOLD_PROJECT_SCOPE: rawScope,
      SCAFFOLD_REQUIRED_CAPABILITIES: oldCapabilities,
      SCAFFOLD_CONTROL_PLANE_URL: "https://authority.invalid",
      COMET_RELEASES_GCS_BUCKET: "synthetic-retained-releases",
      GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "fixture@example.invalid",
      GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: privateKey
    },
    // The one existing BLOBS bucket, intentionally no RELEASES binding.
    r2Buckets: ["BLOBS"],
    durableObjects: Object.fromEntries(Object.entries(classes).map(([binding, className]) => [binding, { className, useSQLite: true }])),
    outboundService: async request => {
      const call = { url: request.url, method: request.method, authorization: request.headers.get("authorization"), body: await request.text() };
      outbound.push(call);
      if (request.url === "https://authority.invalid/api/code-sandboxes/auth/session") {
        return Response.json({ ok: true, resource: "https://authority.invalid", actor: { sub: "owner", auth: "iap" }, scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"] });
      }
      if (request.url === "https://authority.invalid/api/code-sandboxes/sandbox/comet-target/verify") {
        const requested = JSON.parse(call.body);
        if (requested.lifecycleEpoch !== authorityEpoch) return Response.json({ error: "stale_lifecycle" }, { status: 403 });
        const profile = { ...requested, lifecycleEpoch: authorityEpoch, version: authorityVersion, actor: { sub: "owner" } };
        const delayed = delayedAuthority;
        if (delayed?.epoch === requested.lifecycleEpoch) {
          delayedAuthority = undefined;
          delayed.captured();
          await delayed.release;
        }
        return Response.json({ ok: true, profile });
      }
      if (request.url === "https://oauth2.googleapis.com/token") {
        return Response.json({ access_token: "synthetic-gcs-token" });
      }
      if (request.url === "https://storage.googleapis.com/download/storage/v1/b/synthetic-retained-releases/o/releases%2Flatest.txt?alt=media") {
        return new Response(request.method === "HEAD" ? null : "retained release", {
          headers: { "content-type": "text/plain", etag: '"synthetic-release"' }
        });
      }
      // Never permit synthetic coverage to contact a live service.
      throw new Error(`Unexpected outbound request: ${request.method} ${request.url}`);
    }
  };
}

const http = (path: string, init: Parameters<Miniflare["dispatchFetch"]>[1] = {}, scope = nativeScope) =>
  mf.dispatchFetch(`https://edge.invalid${path}`, { ...init, headers: { authorization: `Bearer owner@${scope}`, ...init.headers } });
const post = (body: unknown) => ({ method: "POST", body: JSON.stringify(body) });
async function room(binding: keyof typeof classes, name: string) {
  const ns = await mf.getDurableObjectNamespace(binding);
  return ns.get(ns.idFromName(name));
}

async function issue(binding: "AUTH_GRANTS" | "NATIVE_AUTH_GRANTS", id: string, secret: string, scope: string) {
  const stub = await room(binding, id);
  const response = await stub.fetch("https://grant.internal/issue", {
    ...post({
      grantId: id, ownerUserId: "owner", email: "owner@dev.local", projectId: scope,
      deploymentId: scope, sandboxId: "sandbox", targetDeviceId: "sandbox-device",
      sessionId: randomUUID(), lifecycleEpoch: 1, capabilities: ["session.read"],
      grantHash: createHash("sha256").update(secret).digest("base64url"), issuedAt: Date.now(), grantExpiresAt: Date.now() + 60_000
    }),
    headers: binding === "NATIVE_AUTH_GRANTS" ? { [NATIVE_GRANT_ISSUER_HEADER]: "1" } : {}
  });
  expect(response.status).toBe(200);
  return stub;
}

beforeAll(async () => {
  // Ephemeral synthetic signing key; no credentials or deployed data are read.
  privateKey = generateKeyPairSync("rsa", { modulusLength: 2048, privateKeyEncoding: { type: "pkcs8", format: "pem" }, publicKeyEncoding: { type: "spki", format: "pem" } }).privateKey;
  [activeModules, rollbackModules] = await Promise.all([modulesFor("fixture.ts"), modulesFor("rollback-fixture.ts")]);
});
beforeEach(() => {
  outbound = [];
  authorityEpoch = 1;
  authorityVersion = "scaffold.comet-runtime.v1";
  delayedAuthority = undefined;
  mf = new Miniflare(options());
});
afterEach(async () => { await mf?.dispose(); });

it("exchanges and revokes real same-id grants only in their own generation", async () => {
  const id = randomUUID().replaceAll("-", "");
  const legacy = await issue("AUTH_GRANTS", id, oldSecret, rawScope);
  const native = await issue("NATIVE_AUTH_GRANTS", id, nativeSecret, nativeScope);
  const denied = await http("/auth/device-grants/exchange", post({ grant: `cg1.${id}.${oldSecret}` }));
  expect(denied.status).toBe(401);
  const exchanged = await http("/auth/device-grants/exchange", post({ grant: `cg1.${id}.${nativeSecret}` }));
  expect(exchanged.status).toBe(200);
  const nativeToken = (await exchanged.json() as { accessToken: string }).accessToken;
  // Native HTTP did not consume the old record with the very same grant id.
  const oldExchange = await legacy.fetch("https://grant.internal/exchange", post({ secret: oldSecret }));
  expect(oldExchange.status).toBe(200);
  const oldToken = (await oldExchange.json() as { accessToken: string }).accessToken;
  const refreshed = await legacy.fetch("https://grant.internal/refresh", post({ secret: oldToken.split(".")[2] }));
  expect(refreshed.status).toBe(200);
  const rotatedToken = (await refreshed.json() as { accessToken: string }).accessToken;
  expect(rotatedToken).not.toBe(oldToken);
  expect((await legacy.fetch("https://grant.internal/authenticate", post({ secret: rotatedToken.split(".")[2] }))).status).toBe(200);
  expect((await legacy.fetch("https://grant.internal/revoke", post({ ownerUserId: "owner" }))).status).toBe(200);
  expect((await native.fetch("https://grant.internal/authenticate", post({ secret: nativeToken.split(".")[2] }))).status).toBe(200);
  expect((await legacy.fetch("https://grant.internal/authenticate", post({ secret: rotatedToken.split(".")[2] }))).status).toBe(401);
});

it("routes native HTTP and ChatRoom constructor wake traffic to the new device namespace", async () => {
  const device = randomUUID(), chat = randomUUID();
  const legacy = await room("DEVICE_ROOMS", `d2/${device}`);
  const legacyHeaders = {
    "x-comet-auth-user": "owner", "x-comet-auth-project": rawScope,
    "x-comet-auth-capabilities": oldCapabilities, "x-comet-sync-protocol": "durable-records-v1"
  };
  expect((await legacy.fetch("https://device.internal/status", { headers: legacyHeaders })).status).toBe(404);
  const retainedHost = await legacy.fetch("https://device.internal/ws?role=host", {
    headers: { ...legacyHeaders, "x-comet-internal-device-host-auth": "local", upgrade: "websocket" }
  });
  expect(retainedHost.status).toBe(101);
  retainedHost.webSocket!.accept();
  const retainedNudges: string[] = [];
  retainedHost.webSocket!.addEventListener("message", event => {
    const frame = decodeDeviceFrame(new Uint8Array(event.data as ArrayBuffer));
    if (frame.header.k === "nudge") retainedNudges.push(JSON.parse(new TextDecoder().decode(frame.payload)).chatId);
  });
  const retainedSlot = { sentinel: "retained legacy device state" };
  expect((await legacy.fetch("https://device.internal/sidecar/retained-slot", {
    ...post(retainedSlot), headers: legacyHeaders
  })).status).toBe(200);
  const host = await http(`/device/${device}/ws?role=host&nudgeAck=1`, { headers: { upgrade: "websocket" } });
  expect(host.status).toBe(101);
  host.webSocket!.accept();
  const nativeNudges: string[] = [];
  host.webSocket!.addEventListener("message", event => {
    const frame = decodeDeviceFrame(new Uint8Array(event.data as ArrayBuffer));
    if (frame.header.k === "nudge") nativeNudges.push(JSON.parse(new TextDecoder().decode(frame.payload)).chatId);
  });
  try {
    expect((await http(`/device/${device}/status`)).status).toBe(200);
    expect((await http(`/device/${device}/sidecar/retained-slot`)).status).toBe(404);
    const response = await http(`/chat2/${chat}/rows?device=phone&hostDevice=${device}&batchId=fixture`, { method: "POST", body: new Uint8Array([1, 2, 3]) });
    expect(response.status, await response.text()).toBe(200);
    await expect.poll(() => nativeNudges.includes(chat), { timeout: 10_000 }).toBe(true);
    expect(retainedNudges).not.toContain(chat);
    const retainedStatus = await legacy.fetch("https://device.internal/status", { headers: legacyHeaders });
    expect(retainedStatus.status).toBe(200);
    expect(await retainedStatus.json()).toMatchObject({ hostConnected: true });
    const retainedSidecar = await legacy.fetch("https://device.internal/sidecar/retained-slot", { headers: legacyHeaders });
    expect(retainedSidecar.status).toBe(200);
    expect(await retainedSidecar.json()).toEqual(retainedSlot);
  } finally { host.webSocket!.close(); retainedHost.webSocket!.close(); }
});

it("prefixes native blob put/get/head without reading or overwriting retained raw keys", async () => {
  const bucket = await mf.getR2Bucket("BLOBS");
  const chat = randomUUID(), rawKey = `blob/owner/${chat}/part`;
  await bucket.put(rawKey, "legacy sentinel");
  expect((await http(`/blob/${chat}/part`)).status).toBe(404);
  expect((await http(`/blob/${chat}/part`, { method: "PUT", body: "native bytes" })).status).toBe(200);
  expect(await (await http(`/blob/${chat}/part`)).text()).toBe("native bytes");
  const head = await http(`/blob/${chat}/part`, { method: "HEAD" });
  expect(head.status).toBe(200);
  expect(await head.text()).toBe("");
  expect(await (await bucket.get(rawKey))!.text()).toBe("legacy sentinel");
  expect(await (await bucket.get(`native-v2/${rawKey}`))!.text()).toBe("native bytes");
});

it("wakes an evicted retained SessionRoom alarm with real legacy WASM and raw backup storage", async () => {
  const name = randomUUID(), stub = await room("TEST_ALARM_SESSION", name);
  const armed = await stub.fetch("https://fixture.internal/__arm");
  const { id } = await armed.json() as { id: string };
  await mf.unsafeEvictDurableObject(workerName, "RetainedAlarmSession", { name });
  const bucket = await mf.getR2Bucket("BLOBS");
  const key = `backup/rooms/${id}/latest.loro`;
  await expect.poll(async () => (await bucket.head(key))?.size, { timeout: 10_000 }).toBeGreaterThan(0);
  expect(await bucket.head(`native-v2/${key}`)).toBeNull();
  // The alarm's actual retained implementation generated a materialized snapshot.
  const snapshot = await stub.fetch("https://fixture.internal/snapshot", { headers: {
    "x-comet-auth-user": "owner", "x-comet-auth-project": rawScope,
    "x-comet-auth-capabilities": oldCapabilities, "x-comet-room-kind": "workspace"
  } });
  expect(snapshot.status).toBe(200);
  expect(new Uint8Array(await snapshot.arrayBuffer())).toEqual(new Uint8Array(await (await bucket.get(key))!.arrayBuffer()));
});

it("executes rollback HTTP with raw scope/capabilities and attachment keys while retaining native state", async () => {
  const nativeIdentity = await http("/auth/session");
  expect(nativeIdentity.status).toBe(200);
  expect(await nativeIdentity.json()).toMatchObject({ projectScope: nativeScope, capabilities: nativeCapabilities });
  const id = randomUUID().replaceAll("-", "");
  await issue("NATIVE_AUTH_GRANTS", id, nativeSecret, nativeScope);
  await issue("AUTH_GRANTS", id, oldSecret, rawScope);
  await mf.setOptions(options(rollbackModules));
  expect((await http(`/workspace/${rawScope}/stats`, {}, rawScope)).status).toBe(200);
  expect((await http(`/workspace/${rawScope}/stats`)).status).toBe(401);
  const request = { deploymentId: rawScope, sandboxId: "sandbox", targetDeviceId: "sandbox-device", sessionId: randomUUID(), lifecycleEpoch: 1 };
  const invite = await http("/auth/device-grants", post({ ...request, capabilities: ["session.invite"] }), rawScope);
  expect(invite.status).toBe(403); // Passed capability validation; dev identity cannot verify a sandbox.
  expect(await invite.json()).toEqual({ error: "grant_target_forbidden" });
  const syncWrite = await http("/auth/device-grants", post({ ...request, capabilities: ["session.sync.write"] }), rawScope);
  expect(syncWrite.status).toBe(400);
  expect(await syncWrite.json()).toEqual({ error: "invalid_grant_request" });
  const oldExchange = await http("/auth/device-grants/exchange", post({ grant: `cg1.${id}.${oldSecret}` }), rawScope);
  expect(oldExchange.status).toBe(200);
  const native = await room("NATIVE_AUTH_GRANTS", id);
  expect((await native.fetch("https://grant.internal/exchange", post({ secret: nativeSecret }))).status).toBe(200);
  const body = "retained attachment", digest = createHash("sha256").update(body).digest("hex");
  expect((await http(`/attachments/${digest}`, { method: "PUT", body }, rawScope)).status).toBe(200);
  expect(await (await http(`/attachments/${digest}`, {}, rawScope)).text()).toBe(body);
  const bucket = await mf.getR2Bucket("BLOBS");
  expect(await (await bucket.get(`att/${rawScope}/${digest}`))!.text()).toBe(body);
  expect(await bucket.head(`native-v2/att/${rawScope}/${digest}`)).toBeNull();
  const device = randomUUID();
  const oldHost = await http(`/device/${device}/ws?role=host&syncProtocol=durable-records-v1`, { headers: { upgrade: "websocket" } }, rawScope);
  expect(oldHost.status).toBe(101);
  oldHost.webSocket!.accept();
  try {
    const status = await http(`/device/${device}/status`, {}, rawScope);
    expect(status.status).toBe(200);
    expect(await status.json()).toMatchObject({ hostConnected: true });
    const sidecarPath = `/device/${device}/sidecar/retained-slot`;
    const retainedSlot = { sentinel: "rollback legacy device" };
    expect((await http(`${sidecarPath}?syncProtocol=durable-records-v1`, post(retainedSlot), rawScope)).status).toBe(200);
    const sidecar = await http(sidecarPath, {}, rawScope);
    expect(sidecar.status).toBe(200);
    expect(await sidecar.json()).toEqual(retainedSlot);
    const intruder = await http(`/device/${device}/ws?role=host&syncProtocol=durable-records-v1`, {
      headers: { authorization: `Bearer intruder@${rawScope}`, upgrade: "websocket" }
    }, rawScope);
    expect(intruder.status).toBe(403);
  } finally { oldHost.webSocket!.close(); }
  // All appended class bindings must still instantiate after the entry swap.
  for (const binding of ["NATIVE_DEVICE_ROOMS", "NATIVE_PREVIEW_ROOMS", "NATIVE_REGISTRY_ROOMS", "NATIVE_CHAT_ROOMS"] as const) {
    const response = await (await room(binding, randomUUID())).fetch("https://fixture.internal/not-a-route");
    expect(response.status).toBeGreaterThanOrEqual(400);
    expect(response.status).toBeLessThan(500);
  }
});

it("serves only the exact retained GET/HEAD GCS release route without a new R2 feed", async () => {
  for (const method of ["GET", "HEAD"]) {
    const response = await http("/api/releases/latest.txt", { method }, rawScope);
    expect(response.status).toBe(200);
    expect(response.headers.get("etag")).toBe('"synthetic-release"');
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    expect(await response.text()).toBe(method === "HEAD" ? "" : "retained release");
  }
  expect(outbound.filter(call => new URL(call.url).hostname === "storage.googleapis.com").map(call => [call.method, call.authorization])).toEqual([
    ["GET", "Bearer synthetic-gcs-token"], ["HEAD", "Bearer synthetic-gcs-token"]
  ]);
  for (const call of outbound.filter(call => call.url === "https://oauth2.googleapis.com/token")) {
    expect(call.method).toBe("POST");
    const assertion = new URLSearchParams(call.body).get("assertion")!;
    expect(JSON.parse(Buffer.from(assertion.split(".")[1], "base64url").toString())).toMatchObject({
      iss: "fixture@example.invalid", scope: "https://www.googleapis.com/auth/devstorage.read_only"
    });
  }
  const calls = outbound.length;
  for (const path of ["/releases/latest.txt", "/api/releases/bad%2Ffile", "/api/releases/extra/latest.txt", "/install.sh"]) {
    expect((await http(path)).status).toBe(404);
  }
  expect((await http("/api/releases/latest.txt", { method: "POST" })).status).toBe(404);
  expect(outbound).toHaveLength(calls);
});

it("admits the published v1 authority profile for archive-v2 native grant issuance and rejects unsupported versions", async () => {
  const scaffold = options();
  scaffold.bindings = { ...scaffold.bindings, AUTH_MODE: "scaffold" };
  await mf.setOptions(scaffold);
  const request = { deploymentId: nativeScope, sandboxId: "sandbox", targetDeviceId: "sandbox-device", sessionId: randomUUID(), lifecycleEpoch: 1, capabilities: nativeCapabilities };
  const auth = { authorization: "Bearer sc_rc_synthetic" };
  const verificationName = `scaffold-verification:${createHash("sha256").update(JSON.stringify([
    "sc_rc_synthetic", "https://authority.invalid", nativeScope,
    [...nativeCapabilities].sort(), "local", nativeScope
  ])).digest("base64url")}`;
  const nativeGrants = await mf.getDurableObjectNamespace("NATIVE_AUTH_GRANTS");
  for (const version of ["scaffold.comet-runtime.v2", "unsupported-runtime"]) {
    authorityVersion = version;
    const rejected = await http("/auth/device-grants", { ...post(request), headers: auth });
    expect(rejected.status).toBe(403);
    expect(await rejected.json()).toEqual({ error: "grant_target_forbidden" });
    // Authentication may create only its storage-free coordinator, never a device grant or epoch fence.
    expect((await mf.listDurableObjectIds("NativeAuthGrant")).map(id => id.toString())).toEqual([
      nativeGrants.idFromName(verificationName).toString()
    ]);
    const coordinator = nativeGrants.get(nativeGrants.idFromName(verificationName));
    expect((await coordinator.fetch("https://grant.internal/authenticate", post({ secret: nativeSecret }))).status).toBe(401);
  }
  authorityVersion = "scaffold.comet-runtime.v1";
  const accepted = await http("/auth/device-grants", { ...post(request), headers: auth });
  expect(accepted.status).toBe(200);
  const { grant } = await accepted.json() as { grant: string };
  const exchange = await http("/auth/device-grants/exchange", post({ grant }));
  expect(exchange.status).toBe(200);
  expect(await exchange.json()).toMatchObject({ projectId: nativeScope, deploymentId: nativeScope, sessionId: request.sessionId, capabilities: request.capabilities });
  expect(await mf.listDurableObjectIds("AuthGrant")).toEqual([]);
});

it("retires epoch N native grants after authoritative epoch N+1 admission without touching legacy grants", async () => {
  const scaffold = options();
  scaffold.bindings = { ...scaffold.bindings, AUTH_MODE: "scaffold" };
  await mf.setOptions(scaffold);
  authorityEpoch = 10;
  const sessionId = randomUUID();
  const target = { deploymentId: nativeScope, sandboxId: "sandbox", sessionId, capabilities: nativeCapabilities };
  const scaffoldAuth = { authorization: "Bearer sc_rc_synthetic" };
  const epochRequest = (epoch: number) => ({ ...target, lifecycleEpoch: epoch, targetDeviceId: `comet-scaffold-sandbox-e${epoch}` });
  async function admitEpoch(epoch: number) {
    const admitted = await http("/auth/device-grants", { ...post(epochRequest(epoch)), headers: scaffoldAuth });
    expect(admitted.status).toBe(200);
    const { grant } = await admitted.json() as { grant: string };
    const exchange = await http("/auth/device-grants/exchange", post({ grant }));
    expect(exchange.status).toBe(200);
    const credentials = await exchange.json() as { accessToken: string; lifecycleEpoch: number; targetDeviceId: string };
    expect(credentials).toMatchObject({ lifecycleEpoch: epoch, targetDeviceId: epochRequest(epoch).targetDeviceId });
    return { token: credentials.accessToken, grantId: grant.split(".")[1] };
  }
  const epochN = await admitEpoch(10);
  const staleAuth = { authorization: `Bearer ${epochN.token}` };
  const blobPath = `/blob/${sessionId}/epoch-bound-part`;
  expect((await http(blobPath, { method: "PUT", headers: staleAuth, body: "epoch N bytes" })).status).toBe(200);
  expect(await (await http(blobPath, { headers: staleAuth })).text()).toBe("epoch N bytes");

  // A client claim is not an authoritative transition and cannot retire a valid grant.
  expect((await http("/auth/device-grants", { ...post(epochRequest(11)), headers: scaffoldAuth })).status).toBe(403);
  expect((await http("/auth/session", { headers: staleAuth })).status).toBe(200);

  const legacy = await room("AUTH_GRANTS", epochN.grantId);
  expect((await legacy.fetch("https://grant.internal/issue", post({
    grantId: epochN.grantId, ownerUserId: "owner", email: "owner@dev.local", projectId: rawScope,
    deploymentId: rawScope, sandboxId: target.sandboxId, sessionId, lifecycleEpoch: 10,
    targetDeviceId: epochRequest(10).targetDeviceId, capabilities: ["session.read"],
    grantHash: createHash("sha256").update(oldSecret).digest("base64url"), issuedAt: Date.now(), grantExpiresAt: Date.now() + 60_000
  }))).status).toBe(200);
  const legacyExchange = await legacy.fetch("https://grant.internal/exchange", post({ secret: oldSecret }));
  expect(legacyExchange.status).toBe(200);
  const legacyToken = (await legacyExchange.json() as { accessToken: string }).accessToken;
  const legacyAuthentication = post({ secret: legacyToken.split(".")[2] });
  const legacyBefore = await (await legacy.fetch("https://grant.internal/authenticate", legacyAuthentication)).json();

  const upgraded = await http(`/chat2/${sessionId}/ws?device=${epochRequest(10).targetDeviceId}`, {
    headers: { ...staleAuth, upgrade: "websocket" }
  });
  expect(upgraded.status).toBe(101);
  const socket = upgraded.webSocket!;
  socket.accept();
  const received: number[] = [];
  let closeCode: number | undefined;
  socket.addEventListener("message", event => {
    const frame = decodeFrame(new Uint8Array(event.data as ArrayBuffer));
    if (frame) received.push(frame.type);
  });
  socket.addEventListener("close", event => { closeCode = event.code; });
  try {
    socket.send(encodeFrame(FRAME.hello, { cursor: 0, device: epochRequest(10).targetDeviceId }));
    await expect.poll(() => received.includes(FRAME.state)).toBe(true);
    received.length = 0;

    authorityEpoch = 11;
    const epochNext = await admitEpoch(11);
    const freshAuth = { authorization: `Bearer ${epochNext.token}` };
    // All failures are soft so the initial red run exercises every stale consumer.
    expect.soft((await http("/auth/session", { headers: staleAuth })).status).toBe(401);
    expect.soft((await http(blobPath, { headers: staleAuth })).status).toBe(401);
    expect.soft((await http(blobPath, { method: "PUT", headers: staleAuth, body: "must not overwrite" })).status).toBe(401);
    expect.soft(await (await http(blobPath, { headers: freshAuth })).text()).toBe("epoch N bytes");

    // A silent old reader must be revalidated before delivery, not just on receive.
    const published = await http(`/chat2/${sessionId}/rows?device=${epochRequest(11).targetDeviceId}&batchId=epoch-next`, {
      method: "POST", headers: freshAuth, body: new Uint8Array([11])
    });
    expect(published.status).toBe(200);
    await expect.poll(() => closeCode !== undefined || received.includes(FRAME.row)).toBe(true);
    expect.soft(closeCode).toBe(4401);
    expect.soft(received).not.toContain(FRAME.row);

    // An established old writer must also fail socket revalidation before append.
    if (socket.readyState === 1) {
      socket.send(encodeFrame(FRAME.push, { batchId: "retired-epoch-must-not-append" }, new Uint8Array([10])));
      await expect.poll(() => closeCode !== undefined || received.includes(FRAME.ack)).toBe(true);
    }
    expect.soft(received).not.toContain(FRAME.ack);
    const stats = await http(`/chat2/${sessionId}/stats`, { headers: freshAuth });
    expect(stats.status).toBe(200);
    expect.soft(await stats.json()).toMatchObject({ headSeq: 1, rowCount: 1 });
    expect.soft((await http("/auth/device-grants", { ...post(epochRequest(10)), headers: scaffoldAuth })).status).toBe(403);
    // Check refresh last: a buggy refresh rotates the bearer and would otherwise
    // accidentally invalidate the old socket, masking missing epoch fencing.
    expect.soft((await http("/auth/device-token/refresh", { method: "POST", headers: staleAuth })).status).toBe(401);

    const legacyAfter = await legacy.fetch("https://grant.internal/authenticate", legacyAuthentication);
    expect(legacyAfter.status).toBe(200);
    expect(await legacyAfter.json()).toEqual(legacyBefore);
  } finally { socket.close(); }
});

it("rejects a captured epoch N authority result delivered after epoch N+1 admission", async () => {
  const scaffold = options();
  scaffold.bindings = { ...scaffold.bindings, AUTH_MODE: "scaffold" };
  await mf.setOptions(scaffold);
  authorityEpoch = 20;
  const sessionId = randomUUID();
  const target = { deploymentId: nativeScope, sandboxId: "sandbox", sessionId, capabilities: nativeCapabilities };
  const headers = { authorization: "Bearer sc_rc_synthetic" };
  const captured = Promise.withResolvers<void>();
  const release = Promise.withResolvers<void>();
  delayedAuthority = { epoch: 20, captured: captured.resolve, release: release.promise };
  const olderAdmission = http("/auth/device-grants", {
    ...post({ ...target, lifecycleEpoch: 20, targetDeviceId: "comet-scaffold-sandbox-e20" }), headers
  });
  try {
    // This barrier is after authority verification and profile capture, not a
    // client claim held before the authority has checked its current epoch.
    await Promise.race([
      captured.promise,
      olderAdmission.then(response => { throw new Error(`Older issuance completed before authority capture (${response.status})`); })
    ]);
    authorityEpoch = 21;
    const currentAdmission = await http("/auth/device-grants", {
      ...post({ ...target, lifecycleEpoch: 21, targetDeviceId: "comet-scaffold-sandbox-e21" }), headers
    });
    expect(currentAdmission.status).toBe(200);
    const { grant } = await currentAdmission.json() as { grant: string };
    const exchange = await http("/auth/device-grants/exchange", post({ grant }));
    expect(exchange.status).toBe(200);
    const { accessToken } = await exchange.json() as { accessToken: string };
    release.resolve();
    expect((await olderAdmission).status).not.toBe(200);
    const current = await http("/auth/session", { headers: { authorization: `Bearer ${accessToken}` } });
    expect(current.status).toBe(200);
    expect(await current.json()).toMatchObject({ lifecycleEpoch: 21, targetDeviceId: "comet-scaffold-sandbox-e21" });
  } finally { release.resolve(); await olderAdmission; }
});

it("rejects a different verified device at the current epoch without retiring its bound device", async () => {
  const scaffold = options();
  scaffold.bindings = { ...scaffold.bindings, AUTH_MODE: "scaffold" };
  await mf.setOptions(scaffold);
  authorityEpoch = 30;
  const request = { deploymentId: nativeScope, sandboxId: "sandbox", sessionId: randomUUID(), lifecycleEpoch: 30, capabilities: nativeCapabilities };
  const headers = { authorization: "Bearer sc_rc_synthetic" };
  const admitted = await http("/auth/device-grants", {
    ...post({ ...request, targetDeviceId: "comet-scaffold-sandbox-e30" }), headers
  });
  expect(admitted.status).toBe(200);
  const { grant } = await admitted.json() as { grant: string };
  const exchange = await http("/auth/device-grants/exchange", post({ grant }));
  expect(exchange.status).toBe(200);
  const { accessToken } = await exchange.json() as { accessToken: string };
  const conflicting = await http("/auth/device-grants", {
    ...post({ ...request, targetDeviceId: "comet-scaffold-conflicting-e30" }), headers
  });
  expect(conflicting.status).not.toBe(200);
  const current = await http("/auth/session", { headers: { authorization: `Bearer ${accessToken}` } });
  expect(current.status).toBe(200);
  expect(await current.json()).toMatchObject({ lifecycleEpoch: 30, targetDeviceId: "comet-scaffold-sandbox-e30" });
});

it("denies coordinator operations on an ordinary native grant object", async () => {
  const grant = await room("NATIVE_AUTH_GRANTS", randomUUID().replaceAll("-", ""));
  const scope = {
    ownerUserId: "owner", projectId: nativeScope, deploymentId: nativeScope,
    sessionId: randomUUID(), sandboxId: "sandbox", targetDeviceId: "comet-scaffold-sandbox-e1", lifecycleEpoch: 1
  };
  for (const operation of ["advance", "check"]) {
    const response = await grant.fetch(`https://grant.internal/native-epoch/${operation}`, {
      ...post(scope), headers: { [NATIVE_GRANT_ISSUER_HEADER]: "1" }
    });
    expect(response.status).toBe(403);
  }
});
