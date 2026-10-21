import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { URL as NodeUrl } from "node:url";
import { afterEach, describe, expect, it, vi } from "vitest";
import worker, { SessionRoom, DeviceRoom, AuthGrant } from "./migration-retirement";
import { fetchReleaseObject } from "./release-feed";

vi.mock("./release-feed", () => ({ fetchReleaseObject: vi.fn(async () => new Response("private-release")) }));
const session = "01956000-0000-7000-8000-000000000001";
const migration = "01956000-0000-7000-8000-000000000002";
const otherMigration = "01956000-0000-7000-8000-000000000003";
const origin = "https://comet-staging.internal.ashler.com";
const scope = { ownerSubject: "owner@ashler.ai", projectId: "ashler-staging", deploymentId: "ashler-staging", logicalSessionId: session };
const databases: DatabaseSync[] = [];
const fixture = (workspace = false) => {
  // Node lacks workerd's native fixed-length stream. Runtime HTTP/header behavior
  // is covered by migration-retirement-smoke.mjs; enforce byte bounds here too.
  vi.stubGlobal("FixedLengthStream", class extends TransformStream<Uint8Array, Uint8Array> {
    constructor(length: number) {
      let written = 0;
      super({
        transform(chunk, controller) {
          written += chunk.byteLength;
          if (written > length) throw new Error("fixed_length_overflow");
          controller.enqueue(chunk);
        },
        flush() {
          if (written !== length) throw new Error("fixed_length_underflow");
        }
      });
    }
  });
  const db = new DatabaseSync(":memory:");
  databases.push(db);
  db.exec("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE blobs (name TEXT, idx INTEGER, bytes BLOB, PRIMARY KEY (name, idx)); CREATE TABLE updates (seq INTEGER PRIMARY KEY, received_at INTEGER, bytes BLOB)");
  const originalMeta: Record<string, string> = workspace
    ? { projectScope: scope.projectId, roomKind: "workspace", chatId: `ws4/${scope.projectId}` }
    : { ownerUserId: scope.ownerSubject, projectScope: scope.projectId, chatId: session, extra: 'raw "meta" é 🚀' };
  for (const [key, value] of Object.entries(originalMeta)) db.prepare("INSERT INTO meta VALUES (?, ?)").run(key, value);
  db.prepare("INSERT INTO blobs VALUES (?, ?, ?)").run("snapshot", 1, Buffer.from([255, 0, 128]));
  db.prepare("INSERT INTO blobs VALUES (?, ?, ?)").run("snapshot", 0, Buffer.from([1, 2, 3]));
  db.prepare("INSERT INTO updates VALUES (?, ?, ?)").run(9, 12345, Buffer.from([9, 8, 7]));
  db.prepare("INSERT INTO updates VALUES (?, ?, ?)").run(3, 12340, Buffer.from([6, 5, 4]));
  const socket = { close: vi.fn() } as unknown as WebSocket;
  const sync = vi.fn(async () => {});
  const exec = vi.fn((query: string, ...values: (string | number | ArrayBuffer | Uint8Array)[]) => {
    const rows = db.prepare(query).all(...values.map((value) => value instanceof ArrayBuffer ? new Uint8Array(value) : value));
    return rows.map((row) => Object.fromEntries(Object.entries(row).map(([key, value]) => [key,
      value instanceof Uint8Array ? value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength) : value
    ])));
  });
  const ctx = { storage: { sql: { exec }, sync }, getWebSockets: () => [socket], setWebSocketAutoResponse: vi.fn() } as unknown as DurableObjectState;
  let room = new SessionRoom(ctx);
  const forwarded: Request[] = [];
  const names: string[] = [];
  const bucket = { get: vi.fn(), head: vi.fn(), put: vi.fn(), delete: vi.fn() };
  const env = {
    AUTH_MODE: "scaffold", ENVIRONMENT: "staging", SCAFFOLD_PROJECT_SCOPE: "ashler-staging",
    SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold-staging.internal.ashler.com",
    SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat session.control session.annotate session.invite session.files session.environment",
    COMET_RELEASES_GCS_BUCKET: "private-releases", GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "release-account", GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "private-key",
    BLOBS: bucket,
    SESSION_ROOMS: {
      idFromName: (name: string) => { names.push(name); return name; },
      get: () => ({ fetch: (request: Request) => { forwarded.push(request); return room.fetch(request); } })
    }
  } as unknown as Parameters<typeof worker.fetch>[1];
  const authority = vi.fn(async () => Response.json({
    ok: true, resource: env.SCAFFOLD_CONTROL_PLANE_URL, actor: { sub: " OWNER@ASHLER.AI ", auth: "iap" },
    scopes: ["remote_code:read", "remote_code:exec", "remote_code:write", "remote_code:create"]
  }));
  vi.stubGlobal("fetch", authority);
  const request = (method = "POST", id = migration, options: { path?: string; headers?: Record<string, string>; body?: string } = {}) => new Request(
    options.path ?? `${origin}/session/${session}/migration-export?deploymentId=ashler-staging${method === "GET" ? `&migrationId=${id}` : ""}`,
    { method, headers: { authorization: "Bearer sc_rc_owner", "content-type": "application/json", ...options.headers },
      ...(method === "POST" ? { body: options.body ?? JSON.stringify({ migrationId: id }) } : {}) }
  );
  const source = () => ["meta", "blobs", "updates"].map((table) => db.prepare(`SELECT * FROM ${table} ORDER BY 1, 2`).all());
  return { db, ctx, room, sync, exec, socket, env, bucket, authority, forwarded, names, request, source,
    restart: () => { room = new SessionRoom(ctx); return room; }, call: (req = request()) => worker.fetch(req, env) };
};
afterEach(() => { for (const db of databases.splice(0)) db.close(); vi.unstubAllGlobals(); vi.clearAllMocks(); });

describe("staging legacy retirement export", () => {
  it("authenticates exact staging scope, replaces all caller authority, and retains raw ordered source bytes", async () => {
    const f = fixture();
    const before = f.source();
    const response = await f.call(f.request("POST", migration, { headers: {
      "x-comet-auth-user": "attacker", "x-comet-auth-project": "production", "x-comet-auth-grant": "forged",
      "x-comet-migration-id": otherMigration, "x-comet-migration-session": otherMigration,
      "x-comet-internal-session-owner-auth": "verify", cookie: "private=secret", "x-arbitrary-authority": "yes"
    } }));
    expect(response.status).toBe(200);
    const receipt = await response.json() as { sha256: string; bytes: number; frozenAt: string };
    expect(receipt).toEqual({ version: 1, format: "legacy-session-sql-v1", sourceGeneration: "session", migrationId: migration, scope, frozen: true,
      frozenAt: expect.any(String), sha256: expect.stringMatching(/^[a-f0-9]{64}$/), bytes: expect.any(Number) });
    expect(new Date(receipt.frozenAt).toISOString()).toBe(receipt.frozenAt);
    expect(f.names).toEqual([`s4/ashler-staging/ashler-staging/${session}`]);
    expect(Object.fromEntries(f.forwarded[0]!.headers)).toEqual({
      "x-comet-auth-user": scope.ownerSubject, "x-comet-auth-project": scope.projectId,
      "x-comet-migration-session": session, "x-comet-migration-deployment": scope.deploymentId, "x-comet-migration-id": migration,
      "x-comet-migration-room-kind": "session"
    });
    expect(f.authority.mock.calls).toHaveLength(1);
    const exported = await f.call(f.request("GET"));
    const bytes = Buffer.from(await exported.arrayBuffer());
    expect(exported.headers.get("content-length")).toBe(String(bytes.length));
    expect(exported.headers.get("x-crew-migration-sha256")).toBe(receipt.sha256);
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(receipt.sha256);
    expect(bytes.length).toBe(receipt.bytes);
    expect(JSON.parse(bytes.toString())).toEqual({ schemaVersion: 1, format: "legacy-session-sql-v1", sourceGeneration: "session", migrationId: migration, scope,
      frozenAt: receipt.frozenAt,
      meta: before[0],
      blobs: [{ name: "snapshot", idx: 0, bytesBase64: "AQID" }, { name: "snapshot", idx: 1, bytesBase64: "/wCA" }],
      updates: [{ seq: 3, receivedAt: 12340, bytesBase64: "BgUE" }, { seq: 9, receivedAt: 12345, bytesBase64: "CQgH" }]
    });
    f.restart();
    expect(await (await f.call()).json()).toEqual(receipt);
    expect(Buffer.from(await (await f.call(f.request("GET"))).arrayBuffer())).toEqual(bytes);
    expect((await f.call(f.request("POST", otherMigration))).status).toBe(409);
    expect((await f.call(f.request("GET", otherMigration))).status).toBe(409);
    expect(f.source()).toEqual(before);
    expect(f.sync).toHaveBeenCalled();
    expect(f.socket.close).toHaveBeenCalledWith(1008, "legacy_worker_retired");
  });

  it("rebuilds interrupted chunks with the original freeze and never acknowledges failed persistence", async () => {
    const f = fixture();
    const before = f.source();
    f.sync.mockRejectedValueOnce(new Error("storage ownership lost"));
    await expect(f.call()).rejects.toThrow("storage ownership lost");
    const freeze = f.db.prepare("SELECT * FROM migration_freeze").get()!;
    expect(freeze.receipt).toBeNull();
    expect((await f.call(f.request("GET"))).status).toBe(409);
    f.db.prepare("INSERT INTO migration_export_chunks VALUES (0, ?)").run(Buffer.from("incomplete"));
    f.restart();
    const receipt = await (await f.call()).json() as { frozenAt: string; sha256: string };
    expect(receipt.frozenAt).toBe(freeze.frozen_at);
    const bytes = Buffer.from(await (await f.call(f.request("GET"))).arrayBuffer());
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(receipt.sha256);
    expect(f.source()).toEqual(before);
    // Existing durable-looking receipt still requires a successful sync barrier.
    f.sync.mockRejectedValueOnce(new Error("sync unavailable"));
    await expect(f.call()).rejects.toThrow("sync unavailable");
  });

  it("serializes concurrent requests and accepts bounded opaque migration identifiers", async () => {
    const f = fixture();
    const id = "coordinator_token-1";
    const [first, retry, conflict] = await Promise.all([
      f.call(f.request("POST", id)), f.call(f.request("POST", id)), f.call(f.request("POST", otherMigration))
    ]);
    expect(first.status).toBe(200);
    expect(await retry.json()).toEqual(await first.json());
    expect(conflict.status).toBe(409);
    expect((await f.call(f.request("GET", id))).status).toBe(200);
  });

  it.each([3, 4])("does not acknowledge a failed export persistence barrier %s", async (barrier) => {
    const f = fixture();
    let calls = 0;
    f.sync.mockImplementation(async () => { if (++calls === barrier) throw new Error("disk unavailable"); });
    await expect(f.call()).rejects.toThrow("disk unavailable");
    const frozenAt = f.db.prepare("SELECT frozen_at FROM migration_freeze").get()!.frozen_at;
    f.restart();
    const receipt = await (await f.call()).json() as { frozenAt: string; sha256: string };
    expect(receipt.frozenAt).toBe(frozenAt);
    const bytes = Buffer.from(await (await f.call(f.request("GET"))).arrayBuffer());
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(receipt.sha256);
  });

  it("streams exports beyond 16 MiB through bounded SQL chunks, not KV or full-envelope storage", async () => {
    const f = fixture();
    const blob = Buffer.alloc(96 * 1024, 0xa5);
    for (let idx = 0; idx < 140; idx++) f.db.prepare("INSERT INTO blobs VALUES (?, ?, ?)").run("large-original-snapshot", idx, blob);
    const receipt = await (await f.call()).json() as { bytes: number; sha256: string };
    expect(receipt.bytes).toBeGreaterThan(16 * 1024 * 1024);
    expect(f.db.prepare("SELECT MAX(length(bytes)) AS n FROM migration_export_chunks").get()!.n).toBeLessThanOrEqual(256 * 1024);
    const response = await f.call(f.request("GET"));
    const reader = response.body!.getReader();
    const hash = createHash("sha256");
    let size = 0;
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      expect(value.byteLength).toBeLessThanOrEqual(256 * 1024);
      hash.update(value); size += value.byteLength;
    }
    expect(size).toBe(receipt.bytes);
    expect(hash.digest("hex")).toBe(receipt.sha256);
  });

  it.each(["missing", "extra"])("propagates %s persisted export bytes as a stream error", async (corruption) => {
    const f = fixture();
    const receipt = await (await f.call()).json() as { bytes: number };
    if (corruption === "missing") f.db.prepare("DELETE FROM migration_export_chunks").run();
    else f.db.prepare("UPDATE migration_export_chunks SET bytes = ? WHERE idx = 0").run(Buffer.alloc(receipt.bytes + 1));
    const response = await f.call(f.request("GET"));
    await expect(response.arrayBuffer()).rejects.toThrow();
  });

  it("propagates consumer cancellation without reading the remaining SQL export", async () => {
    const f = fixture();
    const blob = Buffer.alloc(96 * 1024, 0xa5);
    for (let idx = 0; idx < 32; idx++) f.db.prepare("INSERT INTO blobs VALUES (?, ?, ?)").run("cancel-source", idx, blob);
    await f.call();
    const chunks = Number(f.db.prepare("SELECT COUNT(*) AS n FROM migration_export_chunks").get()!.n);
    const response = await f.call(f.request("GET"));
    const reader = response.body!.getReader();
    expect((await reader.read()).done).toBe(false);
    await reader.cancel("consumer disconnected");
    const reads = f.exec.mock.calls.filter(([query]) => query === "SELECT bytes FROM migration_export_chunks WHERE idx = ?").length;
    expect(reads).toBeLessThan(chunks);
  });

  it.each(["ownerUserId", "projectScope", "chatId"])("fails closed for missing or mismatched original %s", async (key) => {
    const f = fixture();
    f.db.prepare("UPDATE meta SET value = 'foreign' WHERE key = ?").run(key);
    expect((await f.call()).status).toBe(403);
    f.db.prepare("DELETE FROM meta WHERE key = ?").run(key);
    expect((await f.call()).status).toBe(403);
    expect(f.db.prepare("SELECT name FROM sqlite_master WHERE name = 'migration_freeze'").all()).toEqual([]);
  });

  it("rejects wrong transports, config, credentials, scopes, and noncanonical routing", async () => {
    const f = fixture();
    for (const token of ["device_token", "local-developer", ""]) expect((await f.call(f.request("POST", migration, { headers: { authorization: `Bearer ${token}` } }))).status).toBe(401);
    expect(f.authority).not.toHaveBeenCalled();
    for (const path of [origin.replace("https:", "http:"), "https://comet.internal.ashler.com"]) expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    for (const change of [{ AUTH_MODE: "dev" }, { ENVIRONMENT: "production" }, { SCAFFOLD_PROJECT_SCOPE: "foreign" }, { SCAFFOLD_CONTROL_PLANE_URL: "https://foreign.example" }, { SCAFFOLD_REQUIRED_CAPABILITIES: "session.read" }]) {
      expect((await worker.fetch(f.request(), { ...f.env, ...change } as typeof f.env)).status).toBe(403);
    }
    for (const suffix of ["", "?deploymentId=production", "?deploymentId=ashler-staging&deploymentId=ashler-staging"]) expect((await f.call(f.request("POST", migration, { path: `${origin}/session/${session}/migration-export${suffix}` }))).status).toBe(400);
    expect((await f.call(f.request("POST", migration, { path: `${origin}/session/AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA/migration-export?deploymentId=ashler-staging` }))).status).toBe(400);
    for (const body of ["null", "[]", "{}", '{"migrationId":"bad/value"}', JSON.stringify({ migrationId: "a".repeat(129) }), JSON.stringify({ migrationId: migration, ownerSubject: "attacker" })]) expect((await f.call(f.request("POST", migration, { body }))).status).toBe(400);
    f.authority.mockImplementation(async () => Response.json({ ok: true, resource: f.env.SCAFFOLD_CONTROL_PLANE_URL, actor: { sub: "owner@ashler.ai", auth: "iap" }, scopes: ["remote_code:read"] }));
    expect((await f.call()).status).toBe(401);
    expect(f.forwarded).toEqual([]);
  });

  it("denies all obsolete Worker/DO writes and hibernated events without changing original tables", async () => {
    const f = fixture();
    const before = f.source();
    for (const path of ["/session/x/ws", "/workspace/ashler-staging/reset-log", "/device/x/nudge", "/auth/device-grants", `/attachments/${"a".repeat(64)}`]) {
      expect((await f.call(f.request("POST", migration, { path: origin + path }))).status).toBe(410);
    }
    for (const Room of [SessionRoom, DeviceRoom, AuthGrant]) {
      const room = new Room(f.ctx);
      for (const method of ["POST", "PUT", "DELETE", "GET"]) expect((await room.fetch(new Request("https://session.internal/append", { method }))).status).toBe(410);
      expect((await room.fetch(new Request("https://session.internal/ws", { headers: { upgrade: "websocket" } }))).status).toBe(410);
      room.webSocketMessage(f.socket, new Uint8Array([1]).buffer);
      room.webSocketError(f.socket, new Error("old socket"));
      room.webSocketClose(f.socket, 1000, "", true);
      await room.alarm();
    }
    expect(f.source()).toEqual(before);
    expect(f.ctx.setWebSocketAutoResponse).toHaveBeenCalledWith();
    expect(f.socket.close).toHaveBeenCalled();
    expect(f.bucket.put).not.toHaveBeenCalled();
    expect(f.bucket.delete).not.toHaveBeenCalled();
  });

  it("keeps only authenticated private releases and content-addressed attachment reads", async () => {
    const f = fixture();
    expect(await (await f.call(f.request("GET", migration, { path: `${origin}/auth/session` }))).json()).toEqual({
      userId: scope.ownerSubject, email: scope.ownerSubject, projectScope: scope.projectId,
      credential: "scaffold", capabilities: ["session.read", "session.control", "session.environment", "session.chat", "session.annotate", "session.files", "session.invite"]
    });
    expect((await f.call(f.request("POST", migration, { path: `${origin}/auth/session` }))).status).toBe(410);
    const file = "/api/releases/desktop-staging-manifest.json";
    expect(await (await f.call(f.request("GET", migration, { path: origin + file }))).text()).toBe("private-release");
    expect(fetchReleaseObject).toHaveBeenCalledWith(f.env, "desktop-staging-manifest.json", "GET");
    expect((await f.call(f.request("GET", migration, { path: origin + file, headers: { authorization: "Bearer device" } }))).status).toBe(401);
    expect(fetchReleaseObject).toHaveBeenCalledTimes(1);
    const hash = "a".repeat(64);
    const object = { size: 19, body: new Blob(["original-attachment"]).stream(), httpEtag: '"original"', writeHttpMetadata: (headers: Headers) => headers.set("content-type", "application/octet-stream") };
    f.bucket.get.mockResolvedValue(object);
    f.bucket.head.mockResolvedValue(object);
    const result = await f.call(f.request("GET", migration, { path: `${origin}/attachments/${hash}` }));
    expect(await result.text()).toBe("original-attachment");
    expect(result.headers.get("etag")).toBe('"original"');
    expect(result.headers.get("content-length")).toBe("19");
    expect(result.headers.get("cache-control")).toBe("private, max-age=31536000, immutable");
    expect(f.bucket.get).toHaveBeenCalledWith(`att/ashler-staging/${hash}`);
    expect((await f.call(f.request("HEAD", migration, { path: `${origin}/attachments/${hash}` }))).body).toBeNull();
    f.authority.mockImplementation(async () => Response.json({ ok: true, resource: f.env.SCAFFOLD_CONTROL_PLANE_URL, actor: { sub: "owner@ashler.ai", auth: "iap" }, scopes: ["remote_code:read", "remote_code:exec"] }));
    expect((await f.call(f.request("GET", migration, { path: `${origin}/attachments/${hash}` }))).status).toBe(401);
  });

  it("exports explicit unscoped personal sources only from s3 with null deployment", async () => {
    const f = fixture();
    const path = `${origin}/session/${session}/migration-export?unscoped=true`;
    const before = f.source();
    const receipt = await (await f.call(f.request("POST", migration, { path, headers: {
      "x-comet-migration-room-kind": "workspace", "x-comet-migration-deployment": scope.deploymentId
    } }))).json() as { scope: unknown; sourceGeneration: string; sha256: string };
    expect(receipt.scope).toEqual({ ...scope, deploymentId: null });
    expect(receipt.sourceGeneration).toBe("session");
    expect(f.names).toEqual([`s3/${scope.projectId}/${session}`]);
    expect(f.forwarded[0]!.headers.get("x-comet-migration-room-kind")).toBe("session-unscoped");
    expect(f.forwarded[0]!.headers.has("x-comet-migration-deployment")).toBe(false);
    const bytes = Buffer.from(await (await f.call(f.request("GET", migration, { path: `${path}&migrationId=${migration}` }))).arrayBuffer());
    expect(JSON.parse(bytes.toString()).scope).toEqual(receipt.scope);
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(receipt.sha256);
    f.restart();
    expect(await (await f.call(f.request("POST", migration, { path }))).json()).toEqual(receipt);
    f.db.prepare("UPDATE meta SET value = 'foreign@ashler.ai' WHERE key = 'ownerUserId'").run();
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    f.db.prepare("UPDATE meta SET value = ? WHERE key = 'ownerUserId'").run(scope.ownerSubject);
    expect(f.source()).toEqual(before);
  });

  it("rejects missing or ambiguous source selectors and never routes local aliases", async () => {
    const f = fixture();
    for (const query of ["", "?unscoped=false", "?unscoped=true&unscoped=true", "?unscoped=true&deploymentId=ashler-staging", "?unscoped=true&deploymentId="]) {
      expect((await f.call(f.request("POST", migration, { path: `${origin}/session/${session}/migration-export${query}` }))).status).toBe(400);
    }
    expect((await f.call(f.request("POST", migration, { path: `${origin}/session/local-chat/migration-export?unscoped=true` }))).status).toBe(400);
    for (const query of ["?deploymentId=ashler-staging", "?unscoped=true", "?unscoped="]) {
      expect((await f.call(f.request("POST", migration, { path: `${origin}/workspace/${scope.projectId}/migration-export${query}` }))).status).toBe(400);
    }
    expect((await f.call(f.request("POST", migration, { path: `${origin}/workspace/foreign/migration-export` }))).status).toBe(400);
    expect(f.forwarded).toEqual([]);
  });

  it.each(["deploymentId", "scopedSessionId"])("rejects conflicting stored personal %s instead of relabeling", async (key) => {
    const f = fixture();
    f.db.prepare("INSERT INTO meta VALUES (?, ?)").run(key, "foreign-binding");
    expect((await f.call()).status).toBe(403);
    expect((await f.call(f.request("POST", migration, { path: `${origin}/session/${session}/migration-export?unscoped=true` }))).status).toBe(403);
    f.db.prepare("UPDATE meta SET value = ? WHERE key = ?").run(key === "deploymentId" ? scope.deploymentId : session, key);
    expect((await f.call()).status).toBe(200);
  });

  it("exports project workspace without fabricating an owner and preserves attribution across collaborators", async () => {
    const f = fixture(true);
    f.db.prepare("INSERT INTO meta VALUES ('owner', ?)").run(scope.projectId);
    const path = `${origin}/workspace/${scope.projectId}/migration-export`;
    const workspaceScope = { projectId: scope.projectId, workspaceId: `ws4/${scope.projectId}`, authority: "project", exportedBySubject: scope.ownerSubject };
    const before = f.source();
    const receipt = await (await f.call(f.request("POST", migration, { path }))).json() as { scope: unknown; sourceGeneration: string; sha256: string };
    expect(receipt.scope).toEqual(workspaceScope);
    expect(receipt.sourceGeneration).toBe("workspace");
    expect(f.names).toEqual([`ws4/${scope.projectId}`]);
    expect(f.forwarded[0]!.headers.get("x-comet-migration-room-kind")).toBe("workspace");
    const bytes = Buffer.from(await (await f.call(f.request("GET", migration, { path: `${path}?migrationId=${migration}` }))).arrayBuffer());
    expect(JSON.parse(bytes.toString())).toMatchObject({ sourceGeneration: "workspace", scope: workspaceScope });
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(receipt.sha256);
    f.authority.mockImplementation(async () => Response.json({ ok: true, resource: f.env.SCAFFOLD_CONTROL_PLANE_URL,
      actor: { sub: "collaborator@ashler.ai", auth: "iap" }, scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec"] }));
    f.restart();
    expect(await (await f.call(f.request("POST", migration, { path }))).json()).toEqual(receipt);
    expect(Buffer.from(await (await f.call(f.request("GET", migration, { path: `${path}?migrationId=${migration}` }))).arrayBuffer())).toEqual(bytes);
    expect((await f.call(f.request("POST", otherMigration, { path }))).status).toBe(409);
    expect(f.source()).toEqual(before);
  });

  it.each(["projectScope", "roomKind", "chatId"])("denies workspace with missing or foreign %s", async (key) => {
    const f = fixture(true);
    const path = `${origin}/workspace/${scope.projectId}/migration-export`;
    f.db.prepare("UPDATE meta SET value = 'foreign' WHERE key = ?").run(key);
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    f.db.prepare("DELETE FROM meta WHERE key = ?").run(key);
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    expect(f.db.prepare("SELECT name FROM sqlite_master WHERE name = 'migration_freeze'").all()).toEqual([]);
  });

  it("allows absent workspace owner metadata but denies conflicting project or personal ownership", async () => {
    const f = fixture(true);
    const path = `${origin}/workspace/${scope.projectId}/migration-export`;
    f.db.prepare("INSERT INTO meta VALUES ('owner', 'foreign-project')").run();
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    f.db.prepare("DELETE FROM meta WHERE key = 'owner'").run();
    f.db.prepare("INSERT INTO meta VALUES ('ownerUserId', ?)").run(scope.ownerSubject);
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(403);
    f.db.prepare("DELETE FROM meta WHERE key = 'ownerUserId'").run();
    expect((await f.call(f.request("POST", migration, { path }))).status).toBe(200);
  });

  it("keeps the original class migrations and excludes every legacy runtime import", () => {
    const source = readFileSync(new NodeUrl("./migration-retirement.ts", import.meta.url), "utf8");
    const config = readFileSync(new NodeUrl("../wrangler.migration-staging.jsonc", import.meta.url), "utf8");
    const parsed = JSON.parse(config.replace(/^\s*\/\/.*$/gm, ""));
    expect(parsed.name).toBe("ashler-comet-edge-staging");
    expect(parsed.main).toBe("src/migration-retirement.ts");
    expect(parsed.env).toBeUndefined();
    expect(parsed.migrations).toEqual([{ tag: "v1", new_sqlite_classes: ["SessionRoom", "DeviceRoom"] }, { tag: "v2", new_sqlite_classes: ["AuthGrant"] }]);
    expect(parsed.durable_objects.bindings).toEqual([{ name: "SESSION_ROOMS", class_name: "SessionRoom" }, { name: "DEVICE_ROOMS", class_name: "DeviceRoom" }, { name: "AUTH_GRANTS", class_name: "AuthGrant" }]);
    expect(parsed.r2_buckets).toEqual([{ binding: "BLOBS", bucket_name: "ashler-comet-staging-blobs" }]);
    expect(parsed.compatibility_flags).toContain("nodejs_compat");
    expect(source).not.toMatch(/from\s+["'][^"']*(?:index|session-room|device-room|auth-routes|grant-authority|loro)["']/);
    expect(source).not.toMatch(/(?:INSERT INTO|UPDATE|DELETE FROM) (?:meta|blobs|updates)\b/);
    expect(source).not.toMatch(/storage\.(?:put|delete|deleteAll|setAlarm)/);
  });
});
