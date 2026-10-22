/**
 * RegistryRoom — one Durable Object per per-user workspace registry
 * (`reg1/{orgId}/{userId}`), the wedge-proof replacement for the Loro
 * workspace doc (docs/registry-sync.md).
 *
 * The DO is the authority: it stores CURRENT row state in its SQLite (no
 * update log, no replay, no wasm), applies pushed ops with per-field LWW
 * (registry-core.ts), bumps one monotonic `seq` per accepted batch, and
 * broadcasts merged rows to every connected socket. Cold start is a table
 * read — the 2026-07/08 wedge class (CPU-limited history replay) cannot
 * exist here by construction.
 *
 * Persistence model:
 * - `rows` — the state. Written synchronously inside the message event (write
 *   rate is index-scale: renames, status flips — never transcript traffic).
 * - `meta` — seq counter, gcFloor, per-device push attribution, backup marks.
 * - presence — memory-only by construction (15s client beats, rebroadcast).
 *
 * Hibernation discipline: ZERO wall-clock timers; ping/pong rides the
 * auto-response pair; the daily alarm does tombstone GC + the R2 backup.
 */
import { applyOp, validateOp, type Op, type Row } from "./registry-core";
import { AUTH_USER_HEADER, apnsConfig, type Env } from "./env";
import { requestAuthority, socketAuthorized, sendAuthorized, type SocketAuthority } from "./authority";
import { bindingMatches, registryHost, scopedOps, visibleRow } from "./registry-scope";
import { isDeadToken, sendApns, type ApnsEnvironment } from "./apns";
import {
  apnsPayload,
  chatForNotification,
  notificationFor,
  parsePrefs,
  type Category,
  type PushPrefs
} from "./push-notify";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Tombstones older than this are purged; cursors from before the purge
 * horizon get a full resync (`gcFloor`). */
const TOMBSTONE_RETAIN_MS = 30 * DAY_MS;
/** Ops per push batch — a cascade delete of a huge space stays comfortably
 * under this; anything larger is split by the client. */
const MAX_BATCH_OPS = 500;
/** Serialized inbound frame budget. */
const MAX_FRAME_BYTES = 1_000_000;

interface SocketState {
  authority?: SocketAuthority;
  userId: string;
  device: string;
  /** Set once a valid hello established the session. */
  ready?: boolean;
}

interface WireOp extends Op {}

interface PushOutcome {
  ok: number;
  rejected: number;
  lastOkAt: number;
}

/** A phone that asked for notifications (APNs token + its choices). */
interface PushTarget {
  device: string;
  token: string;
  environment: ApnsEnvironment;
  prefs: PushPrefs;
}

/** One notification decided by a push batch. */
interface Notification {
  chatId: string;
  title: string;
  category: Category;
}

/** Recent deliveries on /stats (the only place to see them). */
interface PushLogEntry {
  at: number;
  chatId: string;
  category: Category;
  device: string;
  result: string;
}

const TOKEN_RE = /^[0-9a-fA-F]{32,512}$/;
const PUSH_LOG_MAX = 30;

export class RegistryRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  /** device → last presence beat (epoch ms). Memory-only. */
  private readonly presence = new Map<string, number>();
  private send(ws: WebSocket, frame: Record<string, unknown>): void {
    const authority = (ws.deserializeAttachment() as SocketState | null)?.authority;
    if (authority?.credential === "device") {
      if (!this.hostAuthorized(authority)) { ws.close(4403, "registry binding changed"); return; }
      if (frame.t === "presence" && frame.device !== authority.deviceId) return;
      frame = { ...frame };
      if (Array.isArray(frame.rows)) frame.rows = this.filterRows(frame.rows as Row[], authority);
      if (frame.presence) frame.presence = this.visiblePresence(authority);
    }
    sendAuthorized(this.env, this.ctx, ws, JSON.stringify(frame));
  }

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS rows (kind TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL, deleted INTEGER NOT NULL, del_hlc TEXT, fields TEXT NOT NULL, clocks TEXT NOT NULL, PRIMARY KEY (kind, id))"
    );
    ctx.storage.sql.exec("CREATE INDEX IF NOT EXISTS rows_seq ON rows (seq)");
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    // Push targets are private to this room: never rows (those broadcast to
    // every socket, sync to every device and back up to R2).
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS push_targets (device TEXT PRIMARY KEY, token TEXT NOT NULL, environment TEXT NOT NULL, prefs TEXT NOT NULL, updated_at INTEGER NOT NULL)"
    );
    // Same protocol-level keepalive as SessionRoom — and the same caveat: a
    // pong is runtime-answered and proves nothing about this DO's health.
    // Clients judge liveness by probe frames (crates/sync/src/registry.rs).
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }

  private seq(): number {
    return Number(this.getMeta("seq") ?? "0");
  }

  private gcFloor(): number {
    return Number(this.getMeta("gcFloor") ?? "0");
  }

  private hostAuthorized(authority: SocketAuthority): boolean {
    if (!registryHost(authority) || authority.projectId !== this.env.SCAFFOLD_PROJECT_SCOPE ||
        authority.deploymentId !== this.env.SCAFFOLD_DEPLOYMENT_ID ||
        !bindingMatches(this.loadRow("chats", authority.sessionId!), authority)) return false;
    const stored = this.getMeta(`hostBinding/${authority.sessionId}`);
    if (!stored) return true;
    const binding = JSON.parse(stored) as { device: string; epoch: number };
    return authority.lifecycleEpoch! > binding.epoch ||
      authority.lifecycleEpoch === binding.epoch && authority.deviceId === binding.device;
  }

  private filterRows(rows: Row[], authority?: SocketAuthority): Row[] {
    if (authority?.credential !== "device") return rows;
    return rows.filter(row => visibleRow(row, authority));
  }

  private visiblePresence(authority?: SocketAuthority): Record<string, number> {
    if (authority?.credential !== "device") return Object.fromEntries(this.presence);
    const at = this.presence.get(authority.deviceId!);
    return at === undefined ? {} : { [authority.deviceId!]: at };
  }

  // ── row storage ───────────────────────────────────────────────────────────

  private loadRow(kind: string, id: string): Row | undefined {
    const rows = [
      ...this.ctx.storage.sql.exec(
        "SELECT seq, deleted, del_hlc, fields, clocks FROM rows WHERE kind = ? AND id = ?",
        kind,
        id
      )
    ];
    const raw = rows[0];
    if (!raw) return undefined;
    return {
      kind,
      id,
      seq: raw.seq as number,
      deleted: (raw.deleted as number) === 1,
      ...(raw.del_hlc ? { delHlc: raw.del_hlc as string } : {}),
      fields: JSON.parse(raw.fields as string) as Row["fields"],
      clocks: JSON.parse(raw.clocks as string) as Row["clocks"]
    };
  }

  private saveRow(row: Row): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO rows (kind, id, seq, deleted, del_hlc, fields, clocks) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(kind, id) DO UPDATE SET seq = excluded.seq, deleted = excluded.deleted, del_hlc = excluded.del_hlc, fields = excluded.fields, clocks = excluded.clocks",
      row.kind,
      row.id,
      row.seq,
      row.deleted ? 1 : 0,
      row.delHlc ?? null,
      JSON.stringify(row.fields),
      JSON.stringify(row.clocks)
    );
  }

  private rowsSince(cursor: number): Row[] {
    const out: Row[] = [];
    for (const raw of this.ctx.storage.sql.exec(
      "SELECT kind, id, seq, deleted, del_hlc, fields, clocks FROM rows WHERE seq > ? ORDER BY seq",
      cursor
    )) {
      out.push({
        kind: raw.kind as string,
        id: raw.id as string,
        seq: raw.seq as number,
        deleted: (raw.deleted as number) === 1,
        ...(raw.del_hlc ? { delHlc: raw.del_hlc as string } : {}),
        fields: JSON.parse(raw.fields as string) as Row["fields"],
        clocks: JSON.parse(raw.clocks as string) as Row["clocks"]
      });
    }
    return out;
  }

  // ── HTTP surface (only reachable through the authed Worker; org membership
  //    and the per-user room name were enforced there) ──────────────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);
    const authority = requestAuthority(request);
    if (url.pathname === "/peer-target" && request.method === "GET") {
      const authority = requestAuthority(request);
      const sessionId = url.searchParams.get("sessionId") ?? "";
      if (!authority || authority.userId !== userId || !authority.capabilities.includes("session.chat") ||
          authority.projectId !== this.env.SCAFFOLD_PROJECT_SCOPE ||
          authority.deploymentId !== this.env.SCAFFOLD_DEPLOYMENT_ID) return json({ error: "forbidden" }, 403);
      const row = this.loadRow("chats", sessionId);
      const crew = row?.fields.crew;
      if (!row || row.deleted || !crew || typeof crew !== "object" || Array.isArray(crew)) return json({ error: "not_found" }, 404);
      const scope = crew.scope;
      if (crew.ownerSubject !== userId || crew.sessionId !== sessionId ||
          !scope || typeof scope !== "object" || Array.isArray(scope) ||
          scope.projectId !== authority.projectId || scope.deploymentId !== authority.deploymentId || scope.sessionId !== sessionId ||
          typeof row.fields.deviceId !== "string" || !/^[A-Za-z0-9_-]{1,128}$/.test(row.fields.deviceId)) {
        return json({ error: "not_found" }, 404);
      }
      return json({ deviceId: row.fields.deviceId });
    }
    if (authority?.credential === "device") {
      if (authority.userId !== userId || !this.hostAuthorized(authority) ||
          !((request.method === "GET" && ["/ws", "/rows"].includes(url.pathname)) ||
            (request.method === "POST" && url.pathname === "/push")) ||
          (url.searchParams.has("device") && url.searchParams.get("device") !== authority.deviceId)) {
        return json({ error: "forbidden" }, 403);
      }
    }

    if (url.pathname === "/ws") {
      const device = authority?.credential === "device" ? authority.deviceId! : url.searchParams.get("device") ?? "";
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = { userId, device, authority: requestAuthority(request) };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    if (url.pathname === "/stats" && request.method === "GET") {
      const rowCount = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM rows")][0]
        ?.n as number;
      const tombstones = [
        ...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM rows WHERE deleted = 1")
      ][0]?.n as number;
      return json({
        seq: this.seq(),
        gcFloor: this.gcFloor(),
        rowCount,
        tombstones,
        connectedSockets: this.ctx.getWebSockets().length,
        // The ONLY per-device attribution surface — kept from the 2026-08-05
        // incident tooling (SessionRoom's /stats pushOutcomes).
        pushOutcomes: JSON.parse(this.getMeta("pushOutcomes") ?? "{}") as Record<string, PushOutcome>,
        lastBackupSeq: Number(this.getMeta("backupSeq") ?? "0"),
        lastGcAt: Number(this.getMeta("lastGcAt") ?? "0"),
        pushTargets: this.pushTargets().map((t) => ({ device: t.device, environment: t.environment, prefs: t.prefs })),
        pushConfigured: apnsConfig(this.env) !== undefined,
        pushLog: JSON.parse(this.getMeta("pushLog") ?? "[]") as PushLogEntry[]
      });
    }

    // A phone registering (or updating) its APNs token and choices; DELETE on
    // sign-out or when the user turns notifications off.
    if (url.pathname === "/push-target") {
      const device = url.searchParams.get("device") ?? "";
      if (device === "") return json({ error: "bad_request", message: "device required" }, 400);
      if (request.method === "DELETE") {
        this.ctx.storage.sql.exec("DELETE FROM push_targets WHERE device = ?", device);
        return json({ ok: true });
      }
      if (request.method === "POST") {
        let body: Record<string, unknown>;
        try {
          body = (await request.json()) as Record<string, unknown>;
        } catch {
          return json({ error: "bad_request", message: "malformed body" }, 400);
        }
        const token = typeof body.token === "string" ? body.token : "";
        const environment = body.environment === "sandbox" ? "sandbox" : body.environment === "production" ? "production" : undefined;
        if (!TOKEN_RE.test(token) || environment === undefined) {
          return json({ error: "bad_request", message: "token and environment required" }, 400);
        }
        this.ctx.storage.sql.exec(
          "INSERT INTO push_targets (device, token, environment, prefs, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(device) DO UPDATE SET token = excluded.token, environment = excluded.environment, prefs = excluded.prefs, updated_at = excluded.updated_at",
          device,
          token,
          environment,
          JSON.stringify(parsePrefs(body.prefs)),
          Date.now()
        );
        return json({ ok: true });
      }
    }

    if (url.pathname === "/rows" && request.method === "GET") {
      // Pull over plain HTTPS: with `?since=` this is the WS hello's exact
      // delta answer (same full/gcFloor rules); without it, the original
      // full-table repair read. `?device=&beat=1` doubles as a presence
      // beat so an HTTP-only client stays visible to socket peers.
      const sinceRaw = url.searchParams.get("since");
      const sinceNum = sinceRaw === null ? NaN : Number(sinceRaw);
      const cursor = Number.isInteger(sinceNum) && sinceNum >= 0 ? sinceNum : null;
      const device = authority?.credential === "device" ? authority.deviceId! : url.searchParams.get("device") ?? "";
      if (device !== "" && url.searchParams.get("beat") === "1") {
        const at = Date.now();
        this.presence.set(device, at);
        for (const socket of this.ctx.getWebSockets()) {
          const socketState = socket.deserializeAttachment() as SocketState | null;
          if (!socketState?.ready) continue;
          this.send(socket, { t: "presence", device, at });
        }
      }
      const seq = this.seq();
      const gcFloor = this.gcFloor();
      const full = cursor === null || cursor < gcFloor || cursor > seq;
      return json({
        seq,
        full,
        gcFloor,
        rows: this.filterRows(full ? this.rowsSince(0) : this.rowsSince(cursor), authority),
        presence: this.visiblePresence(authority)
      });
    }

    if (url.pathname === "/push" && request.method === "POST") {
      // Push over plain HTTPS — the WS push's fallback twin for networks
      // where the upgrade never completes. Same validation, same atomic
      // apply, same rows broadcast to live sockets; the ack is the response
      // body. LWW clocks make replayed batches apply zero ops, so
      // at-least-once delivery (including 0-RTT/early-data replays) is safe.
      const device = authority?.credential === "device" ? authority.deviceId! : url.searchParams.get("device") ?? "";
      // Pre-read cap (the WS path gets this for free from MAX_FRAME_BYTES):
      // a legitimate batch is bounded well under this by MAX_BATCH_OPS ×
      // MAX_OP_BYTES; anything larger only burns this room's own CPU.
      const declared = Number(request.headers.get("content-length") ?? "0");
      if (declared > 2 * 1024 * 1024) return json({ error: "too_large" }, 413);
      let frame: Record<string, unknown>;
      try {
        frame = (await request.json()) as Record<string, unknown>;
      } catch {
        return json({ error: "bad_push", message: "malformed body" }, 400);
      }
      const outcome = this.applyPushBatch(device, frame, authority);
      if (!outcome.ok) return json({ error: outcome.code, message: outcome.message }, 400);
      return json({ batch: outcome.batch, seq: outcome.seq, applied: outcome.applied });
    }

    if (url.pathname === "/reset" && request.method === "POST") {
      // Operator wipe. Recovery is automatic and lossless: every client
      // detects `state.seq < cursor` on its next hello and re-seeds the table
      // from its local rows with their ORIGINAL clocks (registry-core
      // rowToSeedOp) — the ws4 repair recipe, built in.
      this.ctx.storage.sql.exec("DELETE FROM rows");
      this.ctx.storage.sql.exec("DELETE FROM meta");
      for (const ws of this.ctx.getWebSockets()) {
        try {
          ws.close(4410, "registry reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true });
    }

    return json({ error: "not found" }, 404);
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (!(await socketAuthorized(this.env, ws))) return;
    if (typeof message !== "string") {
      ws.close(1003, "text frames only");
      return;
    }
    if (message.length > MAX_FRAME_BYTES) {
      ws.close(1009, "frame too large");
      return;
    }
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(message) as Record<string, unknown>;
    } catch {
      ws.close(1002, "bad json");
      return;
    }
    if (!frame || typeof frame !== "object" || Array.isArray(frame)) {
      ws.close(1002, "bad frame"); return;
    }
    const state = ws.deserializeAttachment() as SocketState;
    if (state.authority?.credential === "device" && !this.hostAuthorized(state.authority)) {
      ws.close(4403, "registry binding changed"); return;
    }
    switch (frame.t) {
      case "hello":
        this.handleHello(ws, state, frame);
        return;
      case "push":
        this.handlePush(ws, state, frame);
        return;
      case "presence":
        this.handlePresence(ws, state, frame);
        return;
      case "probe":
        this.send(ws, { t: "probe-ok", seq: this.seq() });
        return;
      default:
        this.send(ws, { t: "error", code: "bad_frame", message: `unknown frame: ${String(frame.t)}` });
    }
  }

  async webSocketClose(): Promise<void> {
    /* nothing buffered; rows are written synchronously on push */
  }

  async webSocketError(): Promise<void> {
    /* ditto */
  }

  private handleHello(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    const cursor = typeof frame.cursor === "number" && frame.cursor >= 0 ? frame.cursor : null;
    if (state.authority?.credential === "device") {
      if (frame.device !== undefined && frame.device !== state.authority.deviceId) {
        ws.close(4403, "device mismatch"); return;
      }
      state.device = state.authority.deviceId!;
    } else if (typeof frame.device === "string" && frame.device.length > 0) {
      state.device = frame.device;
    }
    state.ready = true;
    ws.serializeAttachment(state);
    const seq = this.seq();
    const gcFloor = this.gcFloor();
    // A cursor from before the tombstone-GC horizon may have missed purged
    // deletes; a cursor AHEAD of the server means the server lost state (a
    // reset/wipe) — both force `full`, and the client reacts by replacing
    // (former) or re-seeding the server (latter; see registry.rs).
    const full = cursor === null || cursor < gcFloor || cursor > seq;
    const rows = full ? this.rowsSince(0) : this.rowsSince(cursor);
    this.send(ws, {
      t: "state",
      seq,
      full,
      gcFloor,
      rows,
      presence: Object.fromEntries(this.presence)
    });
  }

  private handlePush(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    if (!state.ready) {
      this.recordPush(state.device, false);
      this.send(ws, { t: "error", code: "bad_push", message: "hello first / malformed push" });
      return;
    }
    const outcome = this.applyPushBatch(state.device, frame, state.authority);
    if (!outcome.ok) {
      this.send(ws, { t: "error", code: outcome.code, message: outcome.message });
      return;
    }
    this.send(ws, { t: "ack", batch: outcome.batch, seq: outcome.seq, applied: outcome.applied });
  }

  /** Validate + atomically apply one op batch and broadcast merged rows to
   * every ready socket -- shared by the WS push and the HTTP `POST /push`
   * fallback. The caller delivers the ack/error on its own transport. */
  private applyPushBatch(
    device: string,
    frame: Record<string, unknown>,
    authority?: SocketAuthority
  ):
    | { ok: false; code: string; message: string }
    | { ok: true; batch: string; seq: number; applied: number } {
    if (!frame || typeof frame !== "object" || Array.isArray(frame)) {
      return { ok: false, code: "bad_push", message: "malformed push" };
    }
    const batch = typeof frame.batch === "string" ? frame.batch : "";
    if (batch === "" || !Array.isArray(frame.ops)) {
      this.recordPush(device, false);
      return { ok: false, code: "bad_push", message: "malformed push" };
    }
    let ops = frame.ops as WireOp[];
    if (ops.length === 0 || ops.length > MAX_BATCH_OPS) {
      this.recordPush(device, false);
      return { ok: false, code: "bad_push", message: `batch of ${ops.length} ops` };
    }
    for (const op of ops) {
      if (!op || typeof op !== "object" || op.set === null || op.clocks === null) {
        return { ok: false, code: "invalid_op", message: "malformed op" };
      }
      const invalid = validateOp(op);
      if (invalid) {
        // Reject the WHOLE batch: batches are transactional (cascade deletes)
        // and a client that builds one bad op is a client bug to surface, not
        // to partially apply. Rejections are attributed per device on /stats.
        this.recordPush(device, false);
        return { ok: false, code: "invalid_op", message: `${op.kind}/${op.id}: ${invalid}` };
      }
    }
    if (authority?.credential === "device") {
      if (!this.hostAuthorized(authority)) return { ok: false, code: "forbidden", message: "registry binding changed" };
      const allowed = scopedOps(ops, authority, (kind, id) => this.loadRow(kind, id));
      if (!allowed) return { ok: false, code: "forbidden", message: "outside typed host registry scope" };
      ops = allowed;
      this.setMeta(`hostBinding/${authority.sessionId}`, JSON.stringify({
        device: authority.deviceId, epoch: authority.lifecycleEpoch,
      }));
    }

    // Apply atomically: DO events are single-threaded and SQLite writes in an
    // event commit together, so a mid-batch crash never persists half a batch.
    const nextSeq = this.seq() + 1;
    const touched = new Map<string, Row>();
    /** Each touched session row as it was before this batch. */
    const sessionsBefore = new Map<string, Row | undefined>();
    let applied = 0;
    for (const op of ops) {
      const key = `${op.kind} ${op.id}`;
      const before = touched.get(key) ?? this.loadRow(op.kind, op.id);
      if (op.kind === "sessions" && !sessionsBefore.has(op.id)) sessionsBefore.set(op.id, before);
      const { row, changed } = applyOp(before, op);
      if (!changed || row === undefined) continue;
      applied += 1;
      row.seq = nextSeq;
      touched.set(key, row);
    }
    if (applied > 0) {
      for (const row of touched.values()) this.saveRow(row);
      this.setMeta("seq", String(nextSeq));
      this.markBackupDirty();
    }
    this.recordPush(device, true);
    const seq = applied > 0 ? nextSeq : this.seq();
    if (applied > 0) {
      // Merged full rows to EVERY ready socket (sender included -- its op may
      // have lost LWW, and the merged row is the truth it must display).
      // Rows go out BEFORE the ack so the sender's authoritative state is
      // current by the time the ack retires its optimistic pending batch --
      // no between-frames flicker window.
      const rows = [...touched.values()];
      for (const socket of this.ctx.getWebSockets()) {
        const socketState = socket.deserializeAttachment() as SocketState | null;
        if (!socketState?.ready) continue;
        this.send(socket, { t: "rows", seq, rows });
      }
      this.notifySessions(sessionsBefore, touched, authority);
    }
    return { ok: true, batch, seq, applied };
  }

  // ── push notifications ───────────────────────────────────────────────────

  private pushTargets(): PushTarget[] {
    return [...this.ctx.storage.sql.exec("SELECT device, token, environment, prefs FROM push_targets")].map((raw) => ({
      device: raw.device as string,
      token: raw.token as string,
      environment: (raw.environment as string) === "sandbox" ? "sandbox" : "production",
      prefs: parsePrefs(JSON.parse(raw.prefs as string))
    }));
  }

  /** Session rows that changed in a batch → the desktop's notifications
   * (done / needs input / failed), sent to every registered phone after the
   * batch commits. Best effort: a failed delivery is logged, never retried. */
  private notifySessions(before: Map<string, Row | undefined>, touched: Map<string, Row>, authority?: SocketAuthority): void {
    if (before.size === 0 || !authority?.userId ||
        !authority.projectId || authority.projectId !== this.env.SCAFFOLD_PROJECT_SCOPE ||
        !authority.deploymentId || authority.deploymentId !== this.env.SCAFFOLD_DEPLOYMENT_ID) return;
    const scope = { userId: authority.userId, projectScope: authority.projectId, deploymentId: authority.deploymentId };
    const now = Date.now();
    const notes: Notification[] = [];
    for (const [chatId, prev] of before) {
      const next = touched.get(`sessions ${chatId}`);
      if (!next) continue;
      const category = notificationFor(prev, next, now);
      if (category === null) continue;
      const chat = chatForNotification(touched.get(`chats ${chatId}`) ?? this.loadRow("chats", chatId));
      if (chat === null) continue;
      notes.push({ chatId, title: chat.title, category });
    }
    if (notes.length === 0) return;
    const targets = this.pushTargets();
    const config = apnsConfig(this.env);
    const log: PushLogEntry[] = [];
    const sends: Array<() => Promise<void>> = [];
    for (const note of notes) {
      for (const target of targets) {
        if (!target.prefs[note.category]) continue;
        const entry: PushLogEntry = { at: now, chatId: note.chatId, category: note.category, device: target.device, result: "pending" };
        log.push(entry);
        if (config === undefined) {
          entry.result = "not configured";
          continue;
        }
        sends.push(async () => {
          try {
            const r = await sendApns(config, target.environment, target.token, apnsPayload(note.chatId, note.title, note.category, scope), note.chatId);
            entry.result = r.reason ? `${r.status} ${r.reason}` : String(r.status);
            if (isDeadToken(r)) {
              this.ctx.storage.sql.exec("DELETE FROM push_targets WHERE device = ? AND token = ?", target.device, target.token);
            }
          } catch (err) {
            entry.result = `error ${(err as Error).message}`;
          }
        });
      }
    }
    if (targets.length === 0) {
      for (const note of notes) log.push({ at: now, chatId: note.chatId, category: note.category, device: "", result: "no targets" });
    }
    this.appendPushLog(log);
    if (sends.length > 0) {
      this.ctx.waitUntil(
        Promise.all(sends.map((send) => send())).then(() => this.appendPushLog([], log))
      );
    }
  }

  /** Keep the last few deliveries; `updated` entries (same objects, results
   * filled in after sending) replace their pending copies. */
  private appendPushLog(entries: PushLogEntry[], updated: PushLogEntry[] = []): void {
    let log = JSON.parse(this.getMeta("pushLog") ?? "[]") as PushLogEntry[];
    for (const u of updated) {
      const i = log.findIndex((e) => e.at === u.at && e.chatId === u.chatId && e.device === u.device && e.category === u.category);
      if (i >= 0) log[i] = u;
    }
    log = [...log, ...entries].slice(-PUSH_LOG_MAX);
    this.setMeta("pushLog", JSON.stringify(log));
  }

  private handlePresence(ws: WebSocket, state: SocketState, frame: Record<string, unknown>): void {
    if (!state.ready || state.device === "") return;
    if (state.authority?.credential === "device" && frame.device !== undefined && frame.device !== state.authority.deviceId) {
      ws.close(4403, "device mismatch"); return;
    }
    const at = state.authority?.credential === "device" ? Date.now() : typeof frame.at === "number" ? frame.at : Date.now();
    this.presence.set(state.device, at);
    for (const socket of this.ctx.getWebSockets()) {
      if (socket === ws) continue;
      const socketState = socket.deserializeAttachment() as SocketState | null;
      if (!socketState?.ready) continue;
      this.send(socket, { t: "presence", device: state.device, at });
    }
  }

  private recordPush(device: string, ok: boolean): void {
    const key = device === "" ? "(unknown)" : device;
    const outcomes = JSON.parse(this.getMeta("pushOutcomes") ?? "{}") as Record<string, PushOutcome>;
    const entry = outcomes[key] ?? { ok: 0, rejected: 0, lastOkAt: 0 };
    if (ok) {
      entry.ok += 1;
      entry.lastOkAt = Date.now();
    } else {
      entry.rejected += 1;
    }
    outcomes[key] = entry;
    this.setMeta("pushOutcomes", JSON.stringify(outcomes));
  }

  private markBackupDirty(): void {
    this.setMeta("backupDirty", "1");
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  /** Daily alarm: tombstone GC + nightly R2 backup of the full table. */
  async alarm(): Promise<void> {
    if (this.getMeta("backupDirty") !== "1") return; // idle: stop the chain

    // 1. Tombstone GC. Raising gcFloor to the purged rows' max seq forces a
    //    full resync for any cursor that might have missed a purged delete.
    const horizon = Date.now() - TOMBSTONE_RETAIN_MS;
    const horizonHlc = `${String(horizon).padStart(13, "0")}-`;
    let purgedMaxSeq = 0;
    for (const raw of this.ctx.storage.sql.exec(
      "SELECT seq FROM rows WHERE deleted = 1 AND del_hlc < ?",
      horizonHlc
    )) {
      purgedMaxSeq = Math.max(purgedMaxSeq, raw.seq as number);
    }
    if (purgedMaxSeq > 0) {
      this.ctx.storage.sql.exec("DELETE FROM rows WHERE deleted = 1 AND del_hlc < ?", horizonHlc);
      this.setMeta("gcFloor", String(Math.max(this.gcFloor(), purgedMaxSeq)));
      this.setMeta("lastGcAt", String(Date.now()));
    }

    // 2. Nightly R2 backup — monotonic by seq, so a wiped-and-reseeding room
    //    can never replace the last good copy with a hollow one.
    const seq = this.seq();
    if (seq > Number(this.getMeta("backupSeq") ?? "0")) {
      const rows = this.rowsSince(0);
      await this.env.BLOBS.put(
        `backup/registry/${this.ctx.id.toString()}/latest.json`,
        JSON.stringify({ seq, at: Date.now(), rows })
      );
      this.setMeta("backupSeq", String(seq));
    }
    this.setMeta("backupDirty", "0");
  }
}


const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
