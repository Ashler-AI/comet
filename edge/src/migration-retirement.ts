import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import { authenticateScaffold, bearerFromRequest, credentialTransportAllowed, type ScaffoldAuthEnv } from "./auth";
import { AUTH_PROJECT_HEADER, AUTH_USER_HEADER } from "./env";
import { fetchReleaseObject, type ReleaseFeedEnv } from "./release-feed";
import { scopedSessionRoomKey } from "./room-key";

const PROJECT = "ashler-staging";
const ORIGIN = "https://comet-staging.internal.ashler.com";
const CONTROL_PLANE = "https://scaffold-staging.internal.ashler.com";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const MIGRATION_ID = /^[A-Za-z0-9_-]{1,128}$/;
const SESSION = "x-comet-migration-session";
const DEPLOYMENT = "x-comet-migration-deployment";
const MIGRATION = "x-comet-migration-id";
const ROOM_KIND = "x-comet-migration-room-kind";
const CHUNK_BYTES = 256 * 1024;
const encoder = new TextEncoder();
type MigrationEnv = ScaffoldAuthEnv & ReleaseFeedEnv & { SESSION_ROOMS: DurableObjectNamespace; BLOBS: R2Bucket };
type Scope =
  | { ownerSubject: string; projectId: string; deploymentId: string | null; logicalSessionId: string }
  | { projectId: string; workspaceId: string; authority: "project"; exportedBySubject: string };
type Freeze = { migration_id: string; scope: string; frozen_at: string; receipt: string | null };
type Receipt = {
  version: 1; format: "legacy-session-sql-v1"; sourceGeneration: "session" | "workspace";
  migrationId: string; scope: Scope; frozen: true; frozenAt: string; sha256: string; bytes: number;
};
const json = (body: unknown, status = 200) => Response.json(body, { status, headers: { "cache-control": "private, no-store" } });
const denied = () => json({ error: "legacy_worker_retired" }, 410);

// No legacy runtime, alarm, Loro flush, journal trimming, or metadata repair.
class RetiredRoom implements DurableObject {
  constructor(protected readonly ctx: DurableObjectState) {
    ctx.setWebSocketAutoResponse();
    this.closeSockets();
  }
  protected closeSockets(): void {
    for (const socket of this.ctx.getWebSockets()) this.closeSocket(socket);
  }
  private closeSocket(socket: WebSocket): void {
    try { socket.close(1008, "legacy_worker_retired"); } catch { /* Already closed. */ }
  }
  async fetch(_request: Request): Promise<Response> { this.closeSockets(); return denied(); }
  async alarm(): Promise<void> { this.closeSockets(); }
  webSocketMessage(socket: WebSocket, _message: string | ArrayBuffer): void { this.closeSocket(socket); this.closeSockets(); }
  webSocketClose(socket: WebSocket, _code: number, _reason: string, _wasClean: boolean): void { this.closeSocket(socket); this.closeSockets(); }
  webSocketError(socket: WebSocket, _error: unknown): void { this.closeSocket(socket); this.closeSockets(); }
}
export class DeviceRoom extends RetiredRoom {}
export class AuthGrant extends RetiredRoom {}

export class SessionRoom extends RetiredRoom {
  private pending: Promise<unknown> = Promise.resolve();

  override fetch(request: Request): Promise<Response> {
    // Serialize across sync() barriers, without a 30-second blockConcurrencyWhile
    // timeout for large exports. GET never sees a half-built receipt.
    const result = this.pending.then(() => this.handle(request));
    this.pending = result.catch(() => {});
    return result;
  }

  private async handle(request: Request): Promise<Response> {
    this.closeSockets();
    const url = new URL(request.url);
    if (url.origin !== "https://migration.internal" || url.pathname !== "/migration-export" ||
        !["POST", "GET"].includes(request.method) || request.headers.has("upgrade")) return denied();
    const subject = request.headers.get(AUTH_USER_HEADER) ?? "";
    const projectId = request.headers.get(AUTH_PROJECT_HEADER) ?? "";
    const roomId = request.headers.get(SESSION) ?? "";
    const deploymentId = request.headers.get(DEPLOYMENT);
    const roomKind = request.headers.get(ROOM_KIND);
    const migrationId = request.headers.get(MIGRATION) ?? "";
    if (!subject || projectId !== PROJECT || !MIGRATION_ID.test(migrationId) ||
        (roomKind !== "session" && roomKind !== "session-unscoped" && roomKind !== "workspace") ||
        (roomKind === "workspace" ? roomId !== `ws4/${projectId}` || deploymentId !== null :
          !UUID.test(roomId) || (roomKind === "session" ? deploymentId !== PROJECT : deploymentId !== null))) {
      return json({ error: "invalid_scope" }, 403);
    }
    let scope: Scope = roomKind === "workspace"
      ? { projectId, workspaceId: roomId, authority: "project", exportedBySubject: subject }
      : { ownerSubject: subject, projectId, deploymentId, logicalSessionId: roomId };
    const sql = this.ctx.storage.sql;
    // This original-storage access acquires DO ownership. Cloudflare uniqueness
    // makes replaced instances throw on subsequent storage access, NOT at rollout.
    // https://developers.cloudflare.com/durable-objects/platform/known-issues/
    // Activation requires 100% this version and no rollback while freezes remain;
    // an old handler response is never evidence of a successful freeze.
    const tables = new Set([...sql.exec<{ name: string }>("SELECT name FROM sqlite_master WHERE type = 'table'")].map((row) => row.name));
    if (!["meta", "blobs", "updates"].every((table) => tables.has(table))) return json({ error: "source_unavailable" }, 409);
    const meta = new Map([...sql.exec<{ key: string; value: string }>(
      "SELECT key, value FROM meta WHERE key IN ('ownerUserId', 'owner', 'projectScope', 'chatId', 'roomKind', 'deploymentId', 'scopedSessionId')"
    )].map((row) => [row.key, row.value]));
    if (meta.get("projectScope") !== projectId || meta.get("chatId") !== roomId ||
        (roomKind === "workspace"
          ? meta.get("roomKind") !== "workspace" || meta.has("ownerUserId") ||
            (meta.has("owner") && meta.get("owner") !== projectId)
          : meta.get("ownerUserId") !== subject || meta.get("roomKind") === "workspace" ||
            (meta.has("scopedSessionId") && meta.get("scopedSessionId") !== roomId) ||
            (roomKind === "session-unscoped" ? Boolean(meta.get("deploymentId")) :
              meta.has("deploymentId") && meta.get("deploymentId") !== deploymentId))) {
      return json({ error: "source_owner_scope_denied" }, 403);
    }
    sql.exec("CREATE TABLE IF NOT EXISTS migration_freeze (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), migration_id TEXT NOT NULL, scope TEXT NOT NULL, frozen_at TEXT NOT NULL, receipt TEXT)");
    sql.exec("CREATE TABLE IF NOT EXISTS migration_export_chunks (idx INTEGER PRIMARY KEY, bytes BLOB NOT NULL CHECK (length(bytes) <= 262144))");
    let freeze = [...sql.exec<Freeze>("SELECT migration_id, scope, frozen_at, receipt FROM migration_freeze WHERE singleton = 1")][0];
    if (freeze) {
      const frozenScope = JSON.parse(freeze.scope) as Scope;
      const sameScope = "authority" in scope
        ? "authority" in frozenScope && frozenScope.authority === "project" &&
          frozenScope.projectId === scope.projectId && frozenScope.workspaceId === scope.workspaceId
        : freeze.scope === JSON.stringify(scope);
      if (freeze.migration_id !== migrationId || !sameScope) return json({ error: "migration_conflict" }, 409);
      // Project collaborators retain the original exporter's audit attribution;
      // exportedBySubject is never interpreted as personal ownership.
      scope = frozenScope;
    }
    if (request.method === "GET") {
      if (!freeze?.receipt) return json({ error: "export_not_ready" }, 409);
      await this.ctx.storage.sync();
      return this.stream(JSON.parse(freeze.receipt) as Receipt);
    }
    if (!freeze) {
      freeze = { migration_id: migrationId, scope: JSON.stringify(scope), frozen_at: new Date().toISOString(), receipt: null };
      sql.exec("INSERT INTO migration_freeze (singleton, migration_id, scope, frozen_at) VALUES (1, ?, ?, ?)", migrationId, freeze.scope, freeze.frozen_at);
    }
    // Durable freeze precedes export. Never acknowledge a merely in-memory fence.
    await this.ctx.storage.sync();
    this.closeSockets();
    if (freeze.receipt) return json(JSON.parse(freeze.receipt));
    // A crashed partial export is disposable; original tables remain untouched.
    sql.exec("DELETE FROM migration_export_chunks");
    await this.ctx.storage.sync();
    const hash = createHash("sha256");
    const chunk = new Uint8Array(CHUNK_BYTES);
    let used = 0;
    let index = 0;
    let bytes = 0;
    const flush = async () => {
      if (!used) return;
      sql.exec("INSERT INTO migration_export_chunks (idx, bytes) VALUES (?, ?)", index++, chunk.subarray(0, used));
      await this.ctx.storage.sync();
      used = 0;
    };
    for (const fragment of this.fragments(freeze, scope)) {
      const encoded = encoder.encode(fragment);
      hash.update(encoded);
      bytes += encoded.byteLength;
      if (!Number.isSafeInteger(bytes)) throw new Error("export_size_overflow");
      for (let offset = 0; offset < encoded.length;) {
        const take = Math.min(CHUNK_BYTES - used, encoded.length - offset);
        chunk.set(encoded.subarray(offset, offset + take), used);
        offset += take;
        used += take;
        if (used === CHUNK_BYTES) await flush();
      }
    }
    await flush();
    const receipt: Receipt = {
      version: 1, format: "legacy-session-sql-v1", sourceGeneration: "authority" in scope ? "workspace" : "session", migrationId,
      scope, frozen: true, frozenAt: freeze.frozen_at, sha256: hash.digest("hex"), bytes
    };
    sql.exec("UPDATE migration_freeze SET receipt = ? WHERE singleton = 1", JSON.stringify(receipt));
    await this.ctx.storage.sync();
    return json(receipt);
  }

  private *fragments(freeze: Freeze, scope: Scope): Generator<string> {
    yield `{"schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":${JSON.stringify("authority" in scope ? "workspace" : "session")},"migrationId":${JSON.stringify(freeze.migration_id)},"scope":${JSON.stringify(scope)},"frozenAt":${JSON.stringify(freeze.frozen_at)},"meta":[`;
    let separator = "";
    for (const row of this.ctx.storage.sql.exec<{ key: string; value: string }>("SELECT key, value FROM meta ORDER BY key")) {
      yield separator;
      yield JSON.stringify({ key: row.key, value: row.value });
      separator = ",";
    }
    yield '],"blobs":[';
    separator = "";
    for (const row of this.ctx.storage.sql.exec<{ name: string; idx: number; bytes: ArrayBuffer }>("SELECT name, idx, bytes FROM blobs ORDER BY name, idx")) {
      if (!Number.isSafeInteger(row.idx)) throw new Error("unsafe_blob_index");
      yield `${separator}{"name":${JSON.stringify(row.name)},"idx":${row.idx},"bytesBase64":"`;
      yield* this.base64(row.bytes);
      yield '"}';
      separator = ",";
    }
    yield '],"updates":[';
    separator = "";
    for (const row of this.ctx.storage.sql.exec<{ seq: number; received_at: number; bytes: ArrayBuffer }>("SELECT seq, received_at, bytes FROM updates ORDER BY seq")) {
      if (!Number.isSafeInteger(row.seq) || !Number.isSafeInteger(row.received_at)) throw new Error("unsafe_journal_integer");
      yield `${separator}{"seq":${row.seq},"receivedAt":${row.received_at},"bytesBase64":"`;
      yield* this.base64(row.bytes);
      yield '"}';
      separator = ",";
    }
    yield "]}";
  }

  private *base64(value: ArrayBuffer): Generator<string> {
    const bytes = new Uint8Array(value);
    // Multiples of three avoid padding between slices; never encode an envelope.
    for (let offset = 0; offset < bytes.length; offset += 48 * 1024) {
      const slice = bytes.subarray(offset, offset + 48 * 1024);
      yield Buffer.from(slice.buffer, slice.byteOffset, slice.byteLength).toString("base64");
    }
  }

  private stream(receipt: Receipt): Response {
    let index = 0;
    let bytes = 0;
    const sql = this.ctx.storage.sql;
    const source = new ReadableStream<Uint8Array>({
      pull(controller) {
        const row = [...sql.exec<{ bytes: ArrayBuffer }>("SELECT bytes FROM migration_export_chunks WHERE idx = ?", index++)][0];
        if (!row) {
          if (bytes !== receipt.bytes) controller.error(new Error("export_chunks_incomplete"));
          else controller.close();
          return;
        }
        const chunk = new Uint8Array(row.bytes);
        bytes += chunk.byteLength;
        controller.enqueue(chunk);
      }
    });
    // Workerd derives Content-Length from this native stream, not a header on
    // a generic ReadableStream. Default piping propagates errors and cancellation.
    return new Response(source.pipeThrough(new FixedLengthStream(receipt.bytes)), { encodeBody: "manual", headers: {
      "content-type": "application/json", "cache-control": "private, no-store, no-transform",
      "content-length": String(receipt.bytes), "x-crew-migration-sha256": receipt.sha256
    } });
  }
}

export default {
  async fetch(request: Request, env: MigrationEnv): Promise<Response> {
    const url = new URL(request.url);
    if (env.AUTH_MODE !== "scaffold" || env.ENVIRONMENT !== "staging" ||
        env.SCAFFOLD_PROJECT_SCOPE !== PROJECT || env.SCAFFOLD_CONTROL_PLANE_URL !== CONTROL_PLANE ||
        env.SCAFFOLD_REQUIRED_CAPABILITIES !== "session.read session.chat session.control session.annotate session.invite session.files session.environment" ||
        url.origin !== ORIGIN || !credentialTransportAllowed(url)) return json({ error: "staging_only" }, 403);
    if (request.headers.has("upgrade")) return denied();
    if (!bearerFromRequest(request)?.startsWith("sc_rc_")) return json({ error: "unauthenticated" }, 401);
    const identity = await authenticateScaffold(env, request);
    if (!identity || identity.credential !== "scaffold") return json({ error: "unauthenticated" }, 401);
    if (identity.projectScope !== PROJECT || !identity.userId ||
        !identity.capabilities.includes("session.read") || !identity.capabilities.includes("session.control")) return json({ error: "forbidden" }, 403);
    if (url.pathname === "/auth/session" && request.method === "GET") return json(identity);
    const release = /^\/api\/releases\/([A-Za-z0-9._-]{1,200})$/.exec(url.pathname);
    if (release && (request.method === "GET" || request.method === "HEAD")) return fetchReleaseObject(env, release[1]!, request.method);
    const attachment = /^\/attachments\/([a-f0-9]{64})$/.exec(url.pathname);
    if (attachment && (request.method === "GET" || request.method === "HEAD")) {
      if (!identity.capabilities.includes("session.files")) return json({ error: "forbidden" }, 403);
      const key = `att/${identity.projectScope}/${attachment[1]}`;
      const object = request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
      if (!object) return json({ error: "not_found" }, 404);
      const headers = new Headers();
      object.writeHttpMetadata(headers);
      headers.set("etag", object.httpEtag);
      headers.set("content-length", String(object.size));
      headers.set("cache-control", "private, max-age=31536000, immutable");
      // Content addressed bytes are retained; R2 metadata is not a global freeze.
      return new Response(request.method === "GET" && "body" in object ? (object as R2ObjectBody).body : null, { headers });
    }
    const route = /^\/(session|workspace)\/([^/]+)\/migration-export$/.exec(url.pathname);
    if (!route || !["POST", "GET"].includes(request.method)) return denied();
    const workspace = route[1] === "workspace";
    const deployments = url.searchParams.getAll("deploymentId");
    const unscoped = url.searchParams.getAll("unscoped");
    const sessionId = route[2]!;
    if (workspace
      ? sessionId !== PROJECT || deployments.length !== 0 || unscoped.length !== 0
      : !UUID.test(sessionId) || !(
        (deployments.length === 1 && deployments[0] === PROJECT && unscoped.length === 0) ||
        (deployments.length === 0 && unscoped.length === 1 && unscoped[0] === "true")
      )) return json({ error: "invalid_scope" }, 400);
    let migrationId: unknown;
    if (request.method === "POST") {
      if (request.headers.get("content-type")?.split(";")[0]?.trim() !== "application/json") return json({ error: "expected_json" }, 400);
      try {
        const body: unknown = await request.json();
        if (!body || typeof body !== "object" || Array.isArray(body) || Object.keys(body).length !== 1 || !("migrationId" in body)) return json({ error: "invalid_migration" }, 400);
        migrationId = body.migrationId;
      } catch { return json({ error: "invalid_json" }, 400); }
    } else {
      if (url.searchParams.getAll("migrationId").length !== 1) return json({ error: "invalid_migration" }, 400);
      migrationId = url.searchParams.get("migrationId");
    }
    if (typeof migrationId !== "string" || !MIGRATION_ID.test(migrationId)) return json({ error: "invalid_migration" }, 400);
    // Start empty: ALL caller headers (including trusted auth/migration headers,
    // cookies, bearer, grants and compatibility declarations) are discarded.
    const headers = new Headers({
      [AUTH_USER_HEADER]: identity.userId, [AUTH_PROJECT_HEADER]: identity.projectScope,
      [SESSION]: workspace ? `ws4/${PROJECT}` : sessionId, [MIGRATION]: migrationId,
      [ROOM_KIND]: workspace ? "workspace" : unscoped.length === 1 ? "session-unscoped" : "session"
    });
    if (deployments.length === 1) headers.set(DEPLOYMENT, PROJECT);
    const name = workspace ? `ws4/${identity.projectScope}` : unscoped.length === 1
      ? `s3/${identity.projectScope}/${sessionId}`
      : scopedSessionRoomKey(identity.projectScope, PROJECT, sessionId);
    return env.SESSION_ROOMS.get(env.SESSION_ROOMS.idFromName(name)).fetch(
      new Request("https://migration.internal/migration-export", { method: request.method, headers })
    );
  }
};
