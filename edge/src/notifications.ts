import { isContainer, LoroMap, type LoroDoc } from "loro-crdt";
import { authenticateScaffoldResult } from "./auth";
import type { Env } from "./env";

export interface ApnsEnv {
  APNS_KEY_ID?: string;
  APNS_TEAM_ID?: string;
  APNS_PRIVATE_KEY?: string;
  APNS_TOPIC?: string;
}

export interface NotificationCredentialEnv {
  /** Worker secret: canonical base64 encoding of exactly 32 random bytes. */
  NOTIFICATION_CREDENTIAL_KEY?: string;
}

type Attention = "input" | "error" | "completion";
export type SessionAttentionState = {
  status: string;
  updatedAt: number;
};

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_BODY_BYTES = 2048;
const MAX_USER_DEVICES = 16;
const MAX_PROJECT_DEVICES = 1024;
const REGISTRATION_TTL_MS = 30 * 24 * 60 * 60_000;
// Source-device and Worker clocks can differ by milliseconds. Reject only
// substantial future dates; strict rejection lets the next baseline eat an edge.
const MAX_CLOCK_SKEW_MS = 5_000;
const APNS_DIAGNOSTIC_REASONS: Record<string, true> = {
  BadDeviceToken: true,
  DeviceTokenNotForTopic: true,
  Unregistered: true,
  TopicDisallowed: true,
  InvalidProviderToken: true,
  ExpiredProviderToken: true,
  MissingProviderToken: true
};
const COPY: Record<Attention, string> = {
  input: "A Crew session needs your input.",
  error: "A Crew session encountered an error.",
  completion: "A Crew session finished working."
};

export const attentionTransition = (
  previous: SessionAttentionState | undefined,
  next: SessionAttentionState,
  archived: boolean,
  now: number
): Attention | undefined => {
  if (!previous || archived || next.status === previous.status ||
      next.updatedAt <= previous.updatedAt || next.updatedAt > now + MAX_CLOCK_SKEW_MS ||
      now - next.updatedAt > 45_000) return undefined;
  if (next.status === "awaitingInput") return "input";
  if (next.status === "errored") return "error";
  if (previous.status === "working" && next.status === "idle") return "completion";
  return undefined;
};

type Registration = {
  installationId: string;
  token: string;
  environment: "sandbox" | "production";
};
type RegistrationRequest = Partial<Registration> & Pick<Registration, "installationId">;

export const parseRegistration = (body: unknown, method: string): RegistrationRequest | undefined => {
  if (!body || typeof body !== "object" || Array.isArray(body)) return undefined;
  const value = body as Record<string, unknown>;
  if (typeof value.installationId !== "string" || !UUID.test(value.installationId)) return undefined;
  const installationId = value.installationId.toLowerCase();
  if (method === "DELETE") return { installationId };
  if (method !== "PUT" || typeof value.token !== "string" ||
      !/^(?:[0-9a-f]{2}){16,100}$/.test(value.token) ||
      (value.environment !== "sandbox" && value.environment !== "production")) return undefined;
  return { installationId, token: value.token, environment: value.environment };
};

const json = (value: unknown, status = 200): Response => Response.json(value, { status });
const base64url = (bytes: Uint8Array): string =>
  btoa(String.fromCharCode(...bytes)).replace(/=/g, "").replace(/\+/g, "-").replace(/\//g, "_");
const encodeJson = (value: unknown): string => base64url(new TextEncoder().encode(JSON.stringify(value)));

export class ApnsProvider {
  private cached: { token: string; expiresAt: number } | undefined;
  constructor(private readonly env: ApnsEnv, private readonly transport: typeof fetch = fetch) {}

  async authorization(now = Date.now()): Promise<string> {
    if (this.cached && now < this.cached.expiresAt) return this.cached.token;
    const { APNS_KEY_ID: keyId, APNS_TEAM_ID: teamId, APNS_PRIVATE_KEY: pem, APNS_TOPIC: topic } = this.env;
    if (!keyId || !teamId || !pem || !topic) throw new Error("apns_not_configured");
    if (!/^[A-Z0-9]{10}$/.test(keyId) || !/^[A-Z0-9]{10}$/.test(teamId) ||
        !/^[A-Za-z0-9.-]{1,255}$/.test(topic)) throw new Error("apns_invalid_configuration");
    try {
      const match = pem.match(/^\s*-----BEGIN PRIVATE KEY-----([\s\S]+?)-----END PRIVATE KEY-----\s*$/);
      if (!match) throw new Error("invalid key");
      const der = Uint8Array.from(atob(match[1].replace(/\s/g, "")), (char) => char.charCodeAt(0));
      const key = await crypto.subtle.importKey("pkcs8", der, { name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]);
      const content = `${encodeJson({ alg: "ES256", kid: keyId })}.${encodeJson({ iss: teamId, iat: Math.floor(now / 1000) })}`;
      const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key, new TextEncoder().encode(content));
      const token = `${content}.${base64url(new Uint8Array(signature))}`;
      this.cached = { token, expiresAt: now + 50 * 60_000 };
      return token;
    } catch {
      throw new Error("apns_invalid_configuration");
    }
  }

  async send(device: Registration, chatId: string, projectScope: string, userId: string, attention: Attention, authorization?: string): Promise<{ status: number; apnsId?: string; remove: boolean; invalidatedAt?: number }> {
    const jwt = authorization ?? await this.authorization();
    const host = device.environment === "sandbox" ? "api.sandbox.push.apple.com" : "api.push.apple.com";
    const transport = this.transport;
    const response = await transport(`https://${host}/3/device/${device.token}`, {
      method: "POST",
      headers: {
        authorization: `bearer ${jwt}`,
        "apns-topic": this.env.APNS_TOPIC!,
        "apns-push-type": "alert",
        "apns-priority": "10",
        "apns-expiration": String(Math.floor(Date.now() / 1000) + 45),
        "apns-collapse-id": chatId,
        "content-type": "application/json"
      },
      body: JSON.stringify({
        aps: { alert: { title: "Crew", body: COPY[attention] }, sound: "default" },
        chatId, projectScope, userId
      }),
      signal: AbortSignal.timeout(10_000)
    });
    const status = response.status;
    const apnsId = response.headers.get("apns-id") ?? undefined;
    if (this.env.APNS_TOPIC === "ai.ashler.crew.staging") console.info("Crew push diagnostic", JSON.stringify({ stage: "apns_response", at: Date.now(), status, apnsId }));
    if (response.ok) return { status, apnsId, remove: false };
    const body = await response.json().catch(() => ({})) as { reason?: string; timestamp?: number };
    // Only fixed reason labels may reach logs; never provider bodies or device identifiers.
    const reason = typeof body.reason === "string" && APNS_DIAGNOSTIC_REASONS[body.reason] === true
      ? body.reason : "Other";
    console.warn("Crew push delivery rejected", response.status, reason);
    if (response.status === 410 && body.reason === "Unregistered") {
      return { status, apnsId, remove: true, invalidatedAt: typeof body.timestamp === "number" ? body.timestamp : undefined };
    }
    if (response.status === 400 && (body.reason === "BadDeviceToken" || body.reason === "DeviceTokenNotForTopic")) {
      return { status, apnsId, remove: true };
    }
    if (body.reason === "ExpiredProviderToken") this.cached = undefined;
    return { status, apnsId, remove: false };
  }
}

type StoredDevice = Registration & {
  userId: string;
  revision: string;
  registeredAt: number;
  sealedCredential: string;
};
interface Event { chatId: string; attention: Attention; updatedAt: number }

export class WorkspaceNotifications {
  private readonly provider: ApnsProvider;
  private delivery: Promise<void> = Promise.resolve();
  private credentialKey: Promise<CryptoKey> | undefined;
    private diagnosticQueuedBatches = 0;

    private trace(stage: string, timing: Record<string, string | number | boolean | undefined> = {}): void {
      if (this.env.ENVIRONMENT === "staging") console.info("Crew push diagnostic", JSON.stringify({ stage, at: Date.now(), ...timing }));
    }
  constructor(private readonly storage: DurableObjectStorage, private readonly env: Env) {
    this.provider = new ApnsProvider(env);
    storage.transactionSync(() => {
      const columns = [...storage.sql.exec<{ name: string }>("PRAGMA table_info(notification_devices)")];
      // Old registrations cannot be retained without persisting their plaintext
      // again. Purge the obsolete table and its indexes atomically; clients renew.
      if (columns.some((column) => column.name === "bearer")) {
        storage.sql.exec("DROP TABLE notification_devices");
      }
      storage.sql.exec("CREATE TABLE IF NOT EXISTS notification_devices (installationId TEXT PRIMARY KEY, userId TEXT NOT NULL, token TEXT NOT NULL, environment TEXT NOT NULL, revision TEXT NOT NULL, registeredAt INTEGER NOT NULL, sealedCredential TEXT NOT NULL)");
      storage.sql.exec("CREATE UNIQUE INDEX IF NOT EXISTS notification_token ON notification_devices (token, environment)");
      storage.sql.exec("CREATE INDEX IF NOT EXISTS notification_registration_age ON notification_devices (registeredAt)");
    });
    storage.sql.exec("CREATE TABLE IF NOT EXISTS notification_status (chatId TEXT PRIMARY KEY, status TEXT NOT NULL, updatedAt INTEGER NOT NULL, active INTEGER NOT NULL, edgeAt INTEGER NOT NULL)");
  }

  private encryptionKey(): Promise<CryptoKey> {
    if (!this.credentialKey) {
      const encoded = this.env.NOTIFICATION_CREDENTIAL_KEY;
      if (!encoded || !/^[A-Za-z0-9+/]{43}=$/.test(encoded)) throw new Error("notification_credential_invalid_configuration");
      const bytes = Uint8Array.from(atob(encoded), (char) => char.charCodeAt(0));
      if (bytes.length !== 32 || btoa(String.fromCharCode(...bytes)) !== encoded) throw new Error("notification_credential_invalid_configuration");
      this.credentialKey = crypto.subtle.importKey("raw", bytes, "AES-GCM", false, ["encrypt", "decrypt"]);
    }
    return this.credentialKey;
  }

  private credentialContext(installationId: string, userId: string): Uint8Array {
    const scope = this.env.SCAFFOLD_PROJECT_SCOPE?.trim();
    if (!scope) throw new Error("notification_credential_invalid_configuration");
    return new TextEncoder().encode(JSON.stringify(["crew.notification-credential.v1", installationId, userId, scope]));
  }

  private async sealCredential(installationId: string, userId: string, bearer: string): Promise<string> {
    const additionalData = this.credentialContext(installationId, userId);
    const key = await this.encryptionKey();
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData }, key, new TextEncoder().encode(bearer));
    return `v1.${base64url(iv)}.${base64url(new Uint8Array(ciphertext))}`;
  }

  private async openCredential(device: StoredDevice): Promise<string> {
    const parts = device.sealedCredential.split(".");
    if (parts.length !== 3 || parts[0] !== "v1" || !/^[A-Za-z0-9_-]{16}$/.test(parts[1]) ||
        !/^[A-Za-z0-9_-]+$/.test(parts[2])) throw new Error("notification_credential_invalid");
    const decode = (value: string): Uint8Array => Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/")), (char) => char.charCodeAt(0));
    const additionalData = this.credentialContext(device.installationId, device.userId);
    const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv: decode(parts[1]), additionalData }, await this.encryptionKey(), decode(parts[2]));
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(plaintext);
  }

  async register(request: Request, userId: string, bearer: string): Promise<Response> {
    if (request.method !== "PUT" && request.method !== "DELETE") return json({ error: "method_not_allowed" }, 405);
    if (userId.length > 512 || bearer.length > 8192) return json({ error: "too_large" }, 413);
    // Stream into a fixed upper bound; Content-Length alone is not trustworthy.
    const reader = request.body?.getReader();
    if (!reader) return json({ error: "invalid_request" }, 400);
    const bytes = new Uint8Array(MAX_BODY_BYTES);
    let size = 0;
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        if (size + value.byteLength > MAX_BODY_BYTES) {
          await reader.cancel();
          return json({ error: "too_large" }, 413);
        }
        bytes.set(value, size);
        size += value.byteLength;
      }
    } catch {
      return json({ error: "invalid_request" }, 400);
    }
    let value: RegistrationRequest | undefined;
    try { value = parseRegistration(JSON.parse(new TextDecoder().decode(bytes.subarray(0, size))), request.method); }
    catch { return json({ error: "invalid_request" }, 400); }
    if (!value) return json({ error: "invalid_request" }, 400);
    let sealedCredential: string | undefined;
    if (request.method === "PUT") {
      try { sealedCredential = await this.sealCredential(value.installationId, userId, bearer); }
      catch { return json({ error: "notification_credential_invalid_configuration" }, 503); }
      try { await this.provider.authorization(); }
      catch (error) { return json({ error: error instanceof Error ? error.message : "apns_invalid_configuration" }, 503); }
    }
    // Re-read after every await: ownership and limits are checked in the same
    // synchronous SQL transaction as mutation, including token rotation.
    return this.storage.transactionSync(() => {
      this.storage.sql.exec("DELETE FROM notification_devices WHERE registeredAt < ?", Date.now() - REGISTRATION_TTL_MS);
      const existing = [...this.storage.sql.exec<StoredDevice>("SELECT * FROM notification_devices WHERE installationId = ?", value.installationId)][0];
      if (existing && existing.userId !== userId) return json({ error: "forbidden" }, 403);
      if (request.method === "DELETE") {
        this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ? AND userId = ?", value.installationId, userId);
        return json({ ok: true });
      }
      const tokenOwner = [...this.storage.sql.exec<StoredDevice>("SELECT * FROM notification_devices WHERE token = ? AND environment = ?", value.token!, value.environment!)][0];
      if (tokenOwner && tokenOwner.userId !== userId) return json({ error: "forbidden" }, 403);
      if (!existing && !tokenOwner) {
        const counts = [...this.storage.sql.exec<{ total: number; owned: number }>("SELECT COUNT(*) AS total, COALESCE(SUM(userId = ?), 0) AS owned FROM notification_devices", userId)][0];
        if (counts.total >= MAX_PROJECT_DEVICES || counts.owned >= MAX_USER_DEVICES) return json({ error: "subscription_limit" }, 429);
      }
      if (tokenOwner && tokenOwner.installationId !== value.installationId) {
        this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ?", tokenOwner.installationId);
      }
      this.storage.sql.exec("INSERT INTO notification_devices (installationId, userId, token, environment, revision, registeredAt, sealedCredential) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(installationId) DO UPDATE SET token = excluded.token, environment = excluded.environment, revision = excluded.revision, registeredAt = excluded.registeredAt, sealedCredential = excluded.sealedCredential", value.installationId, userId, value.token!, value.environment!, crypto.randomUUID(), Date.now(), sealedCredential!);
      return json({ ok: true });
    });
  }

  /** Called before import to baseline persisted replay, then after accepted
   * import. Durable monotonic rows prevent heartbeat/replay/hibernation alerts. */
  observe(doc: LoroDoc, baseline: boolean, now = Date.now()): Event[] {
    let phase = "registration_lookup";
    let sessionsMap: LoroMap | undefined;
    let chatsMap: LoroMap | undefined;
    try {
      // No subscribers means no CRDT materialization or per-session SQL writes.
      // The first accepted update after registration still calls the pre-import
      // baseline, silently catching up any status history skipped while absent.
      if (![...this.storage.sql.exec("SELECT 1 FROM notification_devices WHERE registeredAt >= ? LIMIT 1", now - REGISTRATION_TTL_MS)].length) {
        this.trace("observe_skip", { reason: "no_eligible_registration", baseline, observedAt: now });
        return [];
      }
      phase = "sessions_get_map";
      sessionsMap = doc.getMap("sessions");
      phase = "chats_get_map";
      chatsMap = doc.getMap("chats");
      const events: Event[] = [];
      phase = "status_transaction";
      this.storage.transactionSync(() => {
        phase = "status_deactivate";
        this.storage.sql.exec("UPDATE notification_status SET active = 0 WHERE active = 1");
        phase = "session_keys";
        for (const chatId of sessionsMap!.keys()) {
          if (typeof chatId !== "string" || !UUID.test(chatId)) continue;
          const diagnose = chatId === "d8d19839-82fd-44b5-9c5a-7c9ac0106c14";
          phase = "session_get";
          const raw = sessionsMap!.get(chatId);
          let chat: unknown;
          let status: unknown;
          let updatedAt: unknown;
          let archived: unknown;
          try {
            if (!raw || typeof raw !== "object" || (isContainer(raw) && !(raw instanceof LoroMap))) continue;
            phase = "session_fields";
            status = raw instanceof LoroMap ? raw.get("status") : (raw as SessionAttentionState).status;
            updatedAt = raw instanceof LoroMap ? raw.get("updatedAt") : (raw as SessionAttentionState).updatedAt;
            if (typeof status !== "string" || typeof updatedAt !== "number" ||
                !["idle", "working", "awaitingInput", "errored"].includes(status) ||
                !Number.isSafeInteger(updatedAt) || updatedAt > now + MAX_CLOCK_SKEW_MS || updatedAt < 0) {
              if (diagnose && typeof updatedAt === "number" && Number.isSafeInteger(updatedAt) && updatedAt > now + MAX_CLOCK_SKEW_MS) this.trace("observe_skip", { reason: "future", baseline, observedAt: now, updatedAt });
              continue;
            }
            const next: SessionAttentionState = { status, updatedAt };
            phase = "chat_get";
            chat = chatsMap!.get(chatId);
            archived = chat instanceof LoroMap ? chat.get("archived") : chat && typeof chat === "object" && !isContainer(chat) ? (chat as { archived?: unknown }).archived : undefined;
            const active = archived === false;
          phase = "status_lookup";
          const previous = [...this.storage.sql.exec<SessionAttentionState>("SELECT status, updatedAt FROM notification_status WHERE chatId = ?", chatId)][0];
          if (previous && next.updatedAt <= previous.updatedAt) {
            if (diagnose) this.trace("observe_skip", { reason: "not_newer", baseline, observedAt: now, previousUpdatedAt: previous.updatedAt, updatedAt: next.updatedAt });
            phase = "status_reactivate";
            this.storage.sql.exec("UPDATE notification_status SET active = ? WHERE chatId = ?", active ? 1 : 0, chatId);
            continue;
          }
          phase = "attention_decision";
          const attention = baseline ? undefined : attentionTransition(previous, next, !active, now);
          if (diagnose) this.trace("observe", {
            baseline, observedAt: now, updatedAt: next.updatedAt, ageMs: now - next.updatedAt,
            previous: previous?.status ?? "none", next: next.status, active,
            decision: baseline ? "baseline" : !previous ? "initial" : !active ? "inactive" : now - next.updatedAt > 45_000 ? "stale" : attention ?? "non_attention"
          });
          phase = "status_upsert";
          this.storage.sql.exec("INSERT INTO notification_status (chatId, status, updatedAt, active, edgeAt) VALUES (?, ?, ?, ?, ?) ON CONFLICT(chatId) DO UPDATE SET edgeAt = CASE WHEN notification_status.status != excluded.status THEN excluded.edgeAt ELSE notification_status.edgeAt END, status = excluded.status, updatedAt = excluded.updatedAt, active = excluded.active", chatId, next.status, next.updatedAt, active ? 1 : 0, next.updatedAt);
          if (attention) events.push({ chatId, attention, updatedAt: next.updatedAt });
          } finally {
            if (isContainer(raw)) raw.free();
            if (isContainer(chat)) chat.free();
            if (isContainer(status)) status.free();
            if (isContainer(updatedAt)) updatedAt.free();
            if (isContainer(archived)) archived.free();
          }
        }
        phase = "status_commit";
      });
      return events;
    } catch (error) {
      this.trace("observe_exception", { phase, error: (error instanceof Error ? error.message : String(error)).slice(0, 256), stack: error instanceof Error ? error.stack?.split("\n").slice(0, 9).join("\n") : undefined });
      throw new Error(`${phase}: ${(error instanceof Error ? error.message : String(error)).slice(0, 256)}`, { cause: error });
    } finally {
      try { sessionsMap?.free(); } catch (error) { this.trace("observe_cleanup_exception", { phase: "sessions_free", error: (error instanceof Error ? error.message : String(error)).slice(0, 256) }); }
      try { chatsMap?.free(); } catch (error) { this.trace("observe_cleanup_exception", { phase: "chats_free", error: (error instanceof Error ? error.message : String(error)).slice(0, 256) }); }
    }
  }

  deliver(events: Event[], projectScope: string): Promise<void> {
    const queuedAt = Date.now();
    const queuedAhead = this.diagnosticQueuedBatches++;
    this.trace("queue_enqueued", { queuedAt, queuedAhead, events: events.length });
    const delivery = this.delivery.then(() => {
      this.trace("queue_started", { queuedAt, queuedAhead, waitMs: Date.now() - queuedAt });
      return this.deliverInOrder(events, projectScope);
    });
    this.delivery = delivery.catch(() => { console.warn("Crew push delivery failed"); }).finally(() => { this.diagnosticQueuedBatches--; });
    return delivery;
  }

  private async deliverInOrder(events: Event[], projectScope: string): Promise<void> {
    // Commit dedupe before external side effects; delivery failure never rolls
    // back sync or causes a replay to generate another notification.
    const syncStartedAt = Date.now();
    this.trace("storage_sync_started");
    await this.storage.sync();
    this.trace("storage_sync_finished", { elapsedMs: Date.now() - syncStartedAt });
    this.storage.sql.exec("DELETE FROM notification_devices WHERE registeredAt < ?", Date.now() - REGISTRATION_TTL_MS);
    for (const event of events) {
      if (Date.now() - event.updatedAt > 45_000) {
        this.trace("delivery_skipped", { reason: "stale_at_queue_head", updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
        continue;
      }
      const devices = [...this.storage.sql.exec<StoredDevice>("SELECT * FROM notification_devices")];
      this.trace("delivery_started", { updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt, recipients: devices.length });
      // Bounded concurrency avoids a project-sized burst of subrequests.
      for (let offset = 0; offset < devices.length; offset += 8) {
        await Promise.all(devices.slice(offset, offset + 8).map(async (device) => {
          try {
            // Decrypt only for current human-authority introspection. An outage
            // fails closed without deleting a still-valid registration.
            if (projectScope !== this.env.SCAFFOLD_PROJECT_SCOPE.trim()) {
              this.trace("delivery_skipped", { reason: "project_mismatch", updatedAt: event.updatedAt });
              return;
            }
            const authStartedAt = Date.now();
            this.trace("authority_started", { updatedAt: event.updatedAt });
            const bearer = await this.openCredential(device);
            const result = await authenticateScaffoldResult(this.env, new Request("https://crew.internal/notifications/device", {
              headers: { authorization: `Bearer ${bearer}` }
            }));
            this.trace("authority_finished", { updatedAt: event.updatedAt, outcome: result.status, elapsedMs: Date.now() - authStartedAt });
            if (result.status === "invalid") {
              this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ? AND revision = ?", device.installationId, device.revision);
              return;
            }
            if (result.status !== "authenticated") return;
            const identity = result.identity;
            if (identity.userId !== device.userId || identity.projectScope !== projectScope ||
                !identity.capabilities.includes("session.read")) {
              this.trace("delivery_skipped", { reason: "identity_mismatch", updatedAt: event.updatedAt });
              return;
            }
            const authorization = await this.provider.authorization();
            const current = [...this.storage.sql.exec<StoredDevice>("SELECT * FROM notification_devices WHERE installationId = ?", device.installationId)][0];
            const latest = [...this.storage.sql.exec<SessionAttentionState & { active: number; edgeAt: number }>("SELECT status, updatedAt, active, edgeAt FROM notification_status WHERE chatId = ?", event.chatId)][0];
            if (!current || current.revision !== device.revision || !latest?.active || latest.edgeAt !== event.updatedAt ||
                Date.now() - event.updatedAt > 45_000) {
              this.trace("delivery_skipped", { reason: !current ? "unregistered" : current.revision !== device.revision ? "registration_changed" : !latest?.active ? "inactive" : latest.edgeAt !== event.updatedAt ? "superseded" : "stale_before_send", updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
              return;
            }
            this.trace("apns_started", { updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
            const delivery = await this.provider.send(device, event.chatId, projectScope, device.userId, event.attention, authorization);
            this.trace("apns_finished", { updatedAt: event.updatedAt, status: delivery.status, apnsId: delivery.apnsId, removeRegistration: delivery.remove });
            if (delivery.remove && (delivery.invalidatedAt === undefined || device.registeredAt <= delivery.invalidatedAt)) {
              this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ? AND revision = ?", device.installationId, device.revision);
            }
          } catch {
            this.trace("delivery_exception", { updatedAt: event.updatedAt });
            console.warn("Crew push delivery failed");
          }
        }));
      }
    }
  }
}
