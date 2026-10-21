// Local workerd fixture only; never imported by either deployed entrypoint.
import { SessionRoom } from "../src/migration-retirement";
import { scopedSessionRoomKey } from "../src/room-key";

const PROJECT = "ashler-staging";
const OWNER = "owner@ashler.ai";
const TOKEN = "Bearer migration-smoke-local-only";
const BLOB_BYTES = 1_500_000; // Exact original blobs.ts chunk ceiling.
const RECEIVED_AT = 1_800_000_000_000;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
type FixtureEnv = { MIGRATION_SMOKE_LOCAL_ONLY: string; AUTH_MODE: string; ENVIRONMENT: string; ROOMS: DurableObjectNamespace };

export class FixtureSessionRoom extends SessionRoom {
  private fixturePending: Promise<unknown> = Promise.resolve();

  override fetch(request: Request): Promise<Response> {
    // Seeding must finish before an export can obtain its source snapshot.
    const result = this.fixturePending.then(() => this.fixtureRequest(request));
    this.fixturePending = result.catch(() => {});
    return result;
  }

  private async fixtureRequest(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/fixture/")) return super.fetch(request);
    if (url.origin !== "https://migration.internal" || request.headers.get("x-migration-smoke-local-only") !== TOKEN) {
      return new Response("fixture denied", { status: 403 });
    }
    const sql = this.ctx.storage.sql;
    const hasMeta = [...sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'")].length !== 0;
    const hasFreeze = [...sql.exec("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'migration_freeze'")].length !== 0;
    if (url.pathname === "/fixture/seed" && request.method === "POST") {
      if (hasFreeze && [...sql.exec("SELECT singleton FROM migration_freeze WHERE singleton = 1")].length) {
        return new Response("frozen: fixture writes prohibited", { status: 409 });
      }
      if (hasMeta) return new Response("fixture already seeded", { status: 409 });
      const body: unknown = await request.json();
      if (!body || typeof body !== "object" || !("large" in body) || typeof body.large !== "boolean") {
        return new Response("expected fixture size", { status: 400 });
      }
      const kind = request.headers.get("x-comet-migration-room-kind");
      const id = request.headers.get("x-comet-migration-session")!;
      const meta: Record<string, string> = kind === "workspace"
        ? { projectScope: PROJECT, roomKind: "workspace", chatId: id, owner: PROJECT }
        : { ownerUserId: OWNER, projectScope: PROJECT, chatId: id, roomKind: "session" };
      if (kind === "session") {
        meta.deploymentId = PROJECT;
        meta.scopedSessionId = id;
      }
      sql.exec("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
      sql.exec("CREATE TABLE blobs (name TEXT NOT NULL, idx INTEGER NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY (name, idx))");
      sql.exec("CREATE TABLE updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)");
      for (const [key, value] of Object.entries(meta)) sql.exec("INSERT INTO meta (key, value) VALUES (?, ?)", key, value);
      await this.ctx.storage.sync();
      const blob = new Uint8Array(BLOB_BYTES).fill(0xa5);
      const count = body.large ? 96 : 2;
      for (let idx = 0; idx < count; idx++) {
        sql.exec("INSERT INTO blobs (name, idx, bytes) VALUES ('snapshot', ?, ?)", idx, blob);
        await this.ctx.storage.sync();
      }
      sql.exec("INSERT INTO updates (bytes, received_at) VALUES (?, ?)", new Uint8Array(17).fill(0x5a), RECEIVED_AT);
      await this.ctx.storage.sync();
      return Response.json({ seeded: true, blobRows: count });
    }
    if (url.pathname === "/fixture/inspect" && request.method === "GET") {
      if (!hasMeta) return new Response("fixture missing", { status: 404 });
      // Only bounded metadata/aggregate results leave SQL. Full BLOB equality
      // checks every source byte without materializing the room in JS or HTTP.
      const blobs = [...sql.exec(
        "SELECT COUNT(*) AS rows, COALESCE(SUM(length(bytes)), 0) AS bytes, MIN(idx) AS firstIdx, MAX(idx) AS lastIdx, SUM(CASE WHEN name != 'snapshot' OR bytes != ? THEN 1 ELSE 0 END) AS invalidRows FROM blobs",
        new Uint8Array(BLOB_BYTES).fill(0xa5)
      )][0];
      const updates = [...sql.exec(
        "SELECT COUNT(*) AS rows, COALESCE(SUM(length(bytes)), 0) AS bytes, MIN(seq) AS firstSeq, MAX(seq) AS lastSeq, SUM(CASE WHEN bytes != ? OR received_at != ? THEN 1 ELSE 0 END) AS invalidRows FROM updates",
        new Uint8Array(17).fill(0x5a), RECEIVED_AT
      )][0];
      const meta = [...sql.exec("SELECT key, value FROM meta ORDER BY key LIMIT 16")];
      const metaRows = [...sql.exec("SELECT COUNT(*) AS rows FROM meta")][0];
      const chunks = hasFreeze ? [...sql.exec(
        "SELECT COUNT(*) AS rows, COALESCE(SUM(length(bytes)), 0) AS bytes, COALESCE(MAX(length(bytes)), 0) AS maxBytes FROM migration_export_chunks"
      )][0] : { rows: 0, bytes: 0, maxBytes: 0 };
      return Response.json({ source: { meta, metaRows, blobs, updates }, chunks });
    }
    return new Response("fixture operation denied", { status: 410 });
  }
}

export default {
  async fetch(request: Request, env: FixtureEnv): Promise<Response> {
    const url = new URL(request.url);
    if (env.MIGRATION_SMOKE_LOCAL_ONLY !== "true" || env.AUTH_MODE !== "dev" || env.ENVIRONMENT !== "local" ||
        url.protocol !== "http:" || !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) ||
        request.headers.get("authorization") !== TOKEN) return new Response("local fixture only", { status: 403 });
    const route = /^\/fixture\/(session|session-unscoped|workspace)\/([^/]+)\/(seed|inspect|export|legacy-write)$/.exec(url.pathname);
    if (!route) return new Response("fixture route missing", { status: 404 });
    const [, kind, id, action] = route;
    if (kind === "workspace" ? id !== PROJECT : !UUID.test(id!)) return new Response("invalid fixture scope", { status: 400 });
    const subject = request.headers.get("x-smoke-subject") ?? OWNER;
    if (![OWNER, "foreign@ashler.ai"].includes(subject)) return new Response("unknown fixture principal", { status: 403 });
    const headers = new Headers({
      "x-comet-auth-user": subject, "x-comet-auth-project": PROJECT,
      "x-comet-migration-session": kind === "workspace" ? `ws4/${PROJECT}` : id!,
      "x-comet-migration-room-kind": kind!, "x-comet-migration-id": url.searchParams.get("migrationId") ?? "",
      "x-migration-smoke-local-only": TOKEN
    });
    if (kind === "session") headers.set("x-comet-migration-deployment", PROJECT);
    const name = kind === "workspace" ? `ws4/${PROJECT}` : kind === "session-unscoped"
      ? `s3/${PROJECT}/${id}` : scopedSessionRoomKey(PROJECT, PROJECT, id!);
    const path = action === "export" ? "/migration-export" : action === "legacy-write" ? "/append" : `/fixture/${action}`;
    return env.ROOMS.get(env.ROOMS.idFromName(name)).fetch(new Request(`https://migration.internal${path}`, {
      method: request.method, headers, ...(action === "seed" ? { body: request.body } : {})
    }));
  }
};
