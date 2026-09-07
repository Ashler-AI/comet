import { afterEach, describe, expect, it, vi } from "vitest";
import { ApnsProvider, WorkspaceNotifications, attentionTransition, parseRegistration } from "./notifications";
import worker from "./index";
import type { Env } from "./env";

import { DatabaseSync } from "node:sqlite";
import { LoroDoc, LoroMap } from "loro-crdt";
const installationId = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const device = { installationId, token: "ab".repeat(32), environment: "sandbox" as const };
const now = 100_000;
const scaffoldBearer = "sc_rc_private_notification_credential";
const registration = (method = "PUT", value = device) => new Request("https://local/notifications/device", {
  method, body: JSON.stringify(value)
});

const notificationStorage = () => {
  const db = new DatabaseSync(":memory:");
  const storage = {
    sql: { exec: (query: string, ...values: (string | number)[]) => db.prepare(query).all(...values) },
    transactionSync: <T>(body: () => T): T => {
      db.exec("BEGIN");
      try { const result = body(); db.exec("COMMIT"); return result; }
      catch (error) { db.exec("ROLLBACK"); throw error; }
    },
    sync: async () => {}
  } as unknown as DurableObjectStorage;
  return { db, storage };
};

const pendingAttention = (notifications: WorkspaceNotifications) => {
  const doc = new LoroDoc();
  const timestamp = Date.now();
  try {
    const chat = doc.getMap("chats").setContainer(installationId, new LoroMap());
    const session = doc.getMap("sessions").setContainer(installationId, new LoroMap());
    chat.set("archived", false); session.set("status", "working"); session.set("updatedAt", timestamp - 1);
    doc.commit(); notifications.observe(doc, true, timestamp);
    session.set("status", "awaitingInput"); session.set("updatedAt", timestamp); doc.commit();
    return notifications.observe(doc, false, timestamp);
  } finally { doc.free(); }
};

const authorizedSession = () => Response.json({
  ok: true, resource: "https://scaffold-staging.internal.ashler.com",
  actor: { sub: "alice", auth: "iap" },
  scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec"]
});

afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const signingEnv = async () => {
  const keys = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const der = new Uint8Array(await crypto.subtle.exportKey("pkcs8", keys.privateKey));
  return {
    keys,
    env: {
      NOTIFICATION_CREDENTIAL_KEY: btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32)))),
      AUTH_MODE: "scaffold", ENVIRONMENT: "staging",
      SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold-staging.internal.ashler.com",
      SCAFFOLD_PROJECT_SCOPE: "ashler-staging",
      SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat session.control session.annotate session.invite session.files session.environment",
      APNS_KEY_ID: "ABCDEFGHIJ", APNS_TEAM_ID: "0123456789", APNS_TOPIC: "dev.cometnative.Comet",
      APNS_PRIVATE_KEY: `-----BEGIN PRIVATE KEY-----\n${btoa(String.fromCharCode(...der))}\n-----END PRIVATE KEY-----`
    }
  };
};

describe("Crew session attention", () => {
  it("alerts only factual attention edges, never an initial snapshot or heartbeat", () => {
    const previous = { status: "working", updatedAt: now - 1_000 };
    expect(attentionTransition(previous, { status: "awaitingInput", updatedAt: now }, false, now)).toBe("input");
    expect(attentionTransition(previous, { status: "errored", updatedAt: now }, false, now)).toBe("error");
    expect(attentionTransition(previous, { status: "idle", updatedAt: now }, false, now)).toBe("completion");
    expect(attentionTransition(undefined, { status: "errored", updatedAt: now }, false, now)).toBeUndefined();
    expect(attentionTransition({ status: "errored", updatedAt: now - 1 }, { status: "errored", updatedAt: now }, false, now)).toBeUndefined();
    expect(attentionTransition({ status: "awaitingInput", updatedAt: now - 1 }, { status: "idle", updatedAt: now }, false, now)).toBeUndefined();
  });

  it("suppresses archived, stale, reordered and future updates at freshness boundaries", () => {
    const previous = { status: "working", updatedAt: 1 };
    expect(attentionTransition(previous, { status: "idle", updatedAt: now - 45_000 }, false, now)).toBe("completion");
    expect(attentionTransition(previous, { status: "idle", updatedAt: now - 45_001 }, false, now)).toBeUndefined();
    expect(attentionTransition(previous, { status: "idle", updatedAt: 1 }, false, now)).toBeUndefined();
    expect(attentionTransition(previous, { status: "idle", updatedAt: now + 1 }, false, now)).toBeUndefined();
    expect(attentionTransition(previous, { status: "idle", updatedAt: now }, true, now)).toBeUndefined();
  });
});

describe("Crew push registration boundaries", () => {
  it("validates installation, hexadecimal token and explicit APNs environment", () => {
    expect(parseRegistration(device, "PUT")).toEqual(device);
    expect(parseRegistration({ installationId }, "DELETE")).toEqual({ installationId });
    expect(parseRegistration({ ...device, installationId: "../other" }, "PUT")).toBeUndefined();
    expect(parseRegistration({ ...device, token: "AB".repeat(32) }, "PUT")).toBeUndefined();
    expect(parseRegistration({ ...device, token: "ab".repeat(101) }, "PUT")).toBeUndefined();
    expect(parseRegistration({ ...device, environment: "other" }, "PUT")).toBeUndefined();
  });

  it("rejects unauthenticated, cross-project and read-less requests before forwarding", async () => {
    const env = {
      AUTH_MODE: "dev", ENVIRONMENT: "local", SCAFFOLD_PROJECT_SCOPE: "ashler-local",
      SCAFFOLD_REQUIRED_CAPABILITIES: "session.chat"
    } as Env;
    for (const authorization of [undefined, "Bearer alice@other", "Bearer alice@ashler-local"]) {
      const response = await worker.fetch(new Request("http://127.0.0.1/notifications/device", {
        method: "PUT", headers: authorization ? { authorization } : {}, body: JSON.stringify(device)
      }), env);
      expect([401, 403]).toContain(response.status);
    }
  });

  it("keeps registration ownership and status dedupe across room recreation", async () => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    const notifications = new WorkspaceNotifications(storage, env as Env);
    const registration = (method: string, token = device.token) => new Request("https://local/notifications/device", {
      method, body: JSON.stringify({ ...device, token })
    });
    const doc = new LoroDoc();
    try {
      expect((await notifications.register(registration("PUT"), "alice", "bearer")).status).toBe(200);
      expect((await notifications.register(registration("PUT"), "bob", "other")).status).toBe(403);
      expect((await notifications.register(registration("DELETE"), "bob", "other")).status).toBe(403);
      expect((await notifications.register(registration("PUT", "cd".repeat(32)), "alice", "rotated")).status).toBe(200);
      const chat = doc.getMap("chats").setContainer(installationId, new LoroMap());
      const session = doc.getMap("sessions").setContainer(installationId, new LoroMap());
      chat.set("archived", false); session.set("status", "working"); session.set("updatedAt", now - 1);
      doc.commit();
      expect(notifications.observe(doc, true, now)).toEqual([]);
      session.set("status", "idle"); session.set("updatedAt", now); doc.commit();
      expect(notifications.observe(doc, false, now).map((event) => event.attention)).toEqual(["completion"]);
      const resumed = new WorkspaceNotifications(storage, env as Env);
      expect(resumed.observe(doc, false, now)).toEqual([]);
      session.set("updatedAt", now + 1); doc.commit();
      expect(resumed.observe(doc, false, now + 1)).toEqual([]);
      chat.set("archived", true); session.set("status", "errored"); session.set("updatedAt", now + 2); doc.commit();
      expect(resumed.observe(doc, false, now + 2)).toEqual([]);
      expect((await resumed.register(registration("DELETE"), "alice", "bearer")).status).toBe(200);
      expect((await resumed.register(registration("PUT"), "bob", "other")).status).toBe(200);
    } finally { doc.free(); db.close(); }
  });
});

describe("Crew sealed notification credentials", () => {
  it("persists no plaintext credential and decrypts only for current authority checks after recreation", async () => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    const requests: { url: string; authorization: string | null }[] = [];
    vi.stubGlobal("fetch", vi.fn(async (url, init) => {
      requests.push({ url: String(url), authorization: new Headers(init?.headers).get("authorization") });
      return String(url).includes("/auth/session") ? authorizedSession() : new Response(null, { status: 200 });
    }));
    try {
      const notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      const first = db.prepare("SELECT * FROM notification_devices").get()!;
      expect(JSON.stringify(first)).not.toContain(scaffoldBearer);
      expect(first.token).toBe(device.token);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      const renewed = db.prepare("SELECT * FROM notification_devices").get()!;
      expect(JSON.stringify(renewed)).not.toContain(scaffoldBearer);
      expect(renewed.sealedCredential).not.toBe(first.sealedCredential);
      const resumed = new WorkspaceNotifications(storage, env as Env);
      await resumed.deliver(pendingAttention(resumed), env.SCAFFOLD_PROJECT_SCOPE);
      expect(requests.filter((request) => request.url.includes("/auth/session"))).toEqual([
        { url: `${env.SCAFFOLD_CONTROL_PLANE_URL}/api/code-sandboxes/auth/session`, authorization: `Bearer ${scaffoldBearer}` }
      ]);
      expect(requests.filter((request) => request.url.includes("push.apple.com"))).toHaveLength(1);
    } finally { db.close(); }
  });

  it.each(["installation", "user", "project", "ciphertext", "iv", "key"] as const)("fails closed before auth or push when sealed credential %s is changed", async (change) => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    vi.spyOn(console, "warn").mockImplementation(() => {});
    try {
      const notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      if (change === "installation") db.prepare("UPDATE notification_devices SET installationId = ?").run("bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee");
      if (change === "user") db.prepare("UPDATE notification_devices SET userId = ?").run("bob");
      if (change === "project") env.SCAFFOLD_PROJECT_SCOPE = "ashler-production";
      if (change === "key") env.NOTIFICATION_CREDENTIAL_KEY = btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32))));
      if (change === "ciphertext" || change === "iv") {
        const row = db.prepare("SELECT sealedCredential FROM notification_devices").get()!;
        const parts = (row.sealedCredential as string).split(".");
        const index = change === "iv" ? 1 : 2;
        parts[index] = (parts[index][0] === "A" ? "B" : "A") + parts[index].slice(1);
        db.prepare("UPDATE notification_devices SET sealedCredential = ?").run(parts.join("."));
      }
      const resumed = new WorkspaceNotifications(storage, env as Env);
      await resumed.deliver(pendingAttention(resumed), env.SCAFFOLD_PROJECT_SCOPE);
      expect(fetcher).not.toHaveBeenCalled();
      expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(1);
    } finally { db.close(); }
  });

  it.each([undefined, "not-base64", btoa("short"), "A".repeat(42) + "B="])("rejects missing or invalid encryption key %s while DELETE remains usable", async (key) => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    try {
      const notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      const unavailable = new WorkspaceNotifications(storage, { ...env, NOTIFICATION_CREDENTIAL_KEY: key, APNS_PRIVATE_KEY: undefined } as Env);
      expect((await unavailable.register(registration(), "alice", scaffoldBearer)).status).toBe(503);
      expect((await unavailable.register(registration("DELETE"), "alice", scaffoldBearer)).status).toBe(200);
      expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(0);
    } finally { db.close(); }
  });

  it("rejects registration without a project encryption context", async () => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    try {
      const notifications = new WorkspaceNotifications(storage, { ...env, SCAFFOLD_PROJECT_SCOPE: " " } as unknown as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(503);
      expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(0);
    } finally { db.close(); }
  });

  it("purges legacy plaintext registrations and their obsolete column before accepting sealed renewals", async () => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    try {
      db.exec("CREATE TABLE notification_devices (installationId TEXT PRIMARY KEY, userId TEXT NOT NULL, token TEXT NOT NULL, environment TEXT NOT NULL, revision TEXT NOT NULL, registeredAt INTEGER NOT NULL, bearer TEXT NOT NULL)");
      db.exec("CREATE UNIQUE INDEX notification_token ON notification_devices (token, environment)");
      db.exec("CREATE INDEX notification_registration_age ON notification_devices (registeredAt)");
      db.prepare("INSERT INTO notification_devices VALUES (?, ?, ?, ?, ?, ?, ?)").run(installationId, "alice", device.token, device.environment, "old", Date.now(), scaffoldBearer);
      const notifications = new WorkspaceNotifications(storage, env as Env);
      expect(db.prepare("SELECT * FROM notification_devices").all()).toEqual([]);
      expect(db.prepare("PRAGMA table_info(notification_devices)").all().map((column) => column.name)).not.toContain("bearer");
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      new WorkspaceNotifications(storage, env as Env);
      const rows = db.prepare("SELECT * FROM notification_devices").all();
      expect(rows).toHaveLength(1);
      expect(JSON.stringify(rows)).not.toContain(scaffoldBearer);
    } finally { db.close(); }
  });

  it.each([401, 403])("retains registrations through authority outages but deletes a human credential rejected with %s", async (status) => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    let authority: () => Response = authorizedSession;
    const fetcher = vi.fn(async () => authority());
    vi.stubGlobal("fetch", fetcher);
    try {
      const notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      const events = pendingAttention(notifications);
      for (const unavailable of [
        () => new Response(null, { status: 503 }),
        () => new Response(null, { status: 429 }),
        () => { throw new Error("authority unavailable"); },
        () => new Response("not json"),
        () => Response.json(null)
      ]) {
        authority = unavailable;
        await notifications.deliver(events, env.SCAFFOLD_PROJECT_SCOPE);
        expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(1);
      }
      authority = () => new Response(null, { status });
      await notifications.deliver(events, env.SCAFFOLD_PROJECT_SCOPE);
      expect(db.prepare("SELECT * FROM notification_devices").all()).toEqual([]);
      expect(fetcher.mock.calls).toHaveLength(6);
    } finally { db.close(); }
  });

  it("does not delete a renewed registration when its old credential is revoked in flight", async () => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    try {
      let notifications: WorkspaceNotifications;
      let renewalStatus: number | undefined;
      vi.stubGlobal("fetch", vi.fn(async () => {
        renewalStatus = (await notifications.register(registration(), "alice", "sc_rc_renewed")).status;
        return new Response(null, { status: 401 });
      }));
      notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      await notifications.deliver(pendingAttention(notifications), env.SCAFFOLD_PROJECT_SCOPE);
      expect(renewalStatus).toBe(200);
      expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(1);
    } finally { db.close(); }
  });

  it.each(["older-timestamp", "renewed-revision", "current"] as const)("applies APNs 410 only to the invalidated registration: %s", async (invalidation) => {
    const { db, storage } = notificationStorage();
    const { env } = await signingEnv();
    let notifications: WorkspaceNotifications;
    let renewalStatus: number | undefined;
    let pushes = 0;
    vi.stubGlobal("fetch", vi.fn(async (url) => {
      if (String(url).includes("/auth/session")) return authorizedSession();
      pushes += 1;
      const registeredAt = db.prepare("SELECT registeredAt FROM notification_devices").get()!.registeredAt as number;
      if (invalidation === "renewed-revision") {
        renewalStatus = (await notifications.register(registration(), "alice", "sc_rc_renewed")).status;
      }
      return Response.json({ reason: "Unregistered", timestamp: registeredAt - (invalidation === "older-timestamp" ? 1 : 0) }, { status: 410 });
    }));
    try {
      notifications = new WorkspaceNotifications(storage, env as Env);
      expect((await notifications.register(registration(), "alice", scaffoldBearer)).status).toBe(200);
      await notifications.deliver(pendingAttention(notifications), env.SCAFFOLD_PROJECT_SCOPE);
      expect(pushes).toBe(1);
      if (invalidation === "renewed-revision") expect(renewalStatus).toBe(200);
      expect(db.prepare("SELECT COUNT(*) AS count FROM notification_devices").get()!.count).toBe(invalidation === "current" ? 0 : 1);
    } finally { db.close(); }
  });
});

describe("APNs provider", () => {
  it("signs a verifiable ES256 JWT and delivers generic scoped alert payloads", async () => {
    const { env, keys } = await signingEnv();
    let payload: Record<string, unknown> | undefined;
    let headers: Headers | undefined;
    const provider = new ApnsProvider(env, (async (_url, init) => {
      payload = JSON.parse(init!.body as string);
      headers = new Headers(init!.headers);
      return new Response(null, { status: 200 });
    }) as typeof fetch);
    await provider.send(device, installationId, "project", "alice", "input");
    expect(payload).toEqual({ aps: { alert: { title: "Crew", body: "A Crew session needs your input." }, sound: "default" }, chatId: installationId, projectScope: "project", userId: "alice" });
    expect(headers!.get("apns-collapse-id")).toBe(installationId);
    const [header, claims, signature] = headers!.get("authorization")!.slice(7).split(".");
    const bytes = Uint8Array.from(atob(signature.replace(/-/g, "+").replace(/_/g, "/")), (char) => char.charCodeAt(0));
    expect(await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, keys.publicKey, bytes, new TextEncoder().encode(`${header}.${claims}`))).toBe(true);
  });

  it("removes only terminal device errors, not provider failures or throttling", async () => {
    const { env } = await signingEnv();
    for (const [status, reason, remove] of [
      [410, "Unregistered", true], [400, "BadDeviceToken", true],
      [400, "DeviceTokenNotForTopic", true], [403, "ExpiredProviderToken", false],
      [429, "TooManyRequests", false], [500, "InternalServerError", false]
    ] as const) {
      const provider = new ApnsProvider(env, (async () => Response.json({ reason, timestamp: 123 }, { status })) as typeof fetch);
      expect((await provider.send(device, installationId, "project", "alice", "completion")).remove).toBe(remove);
    }
  });

  it("reports terminal APNs reasons without leaking arbitrary provider content", async () => {
    const log = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { env } = await signingEnv();
    for (const reason of ["BadDeviceToken", "InvalidProviderToken", "private-provider-content"]) {
      const provider = new ApnsProvider(env, (async () => Response.json({
        reason, token: device.token, detail: "private-provider-content"
      }, { status: reason === "BadDeviceToken" ? 400 : 403 })) as typeof fetch);
      await provider.send(device, installationId, "project", "alice", "input");
    }
    const messages = log.mock.calls.flat();
    expect(messages).toContain("BadDeviceToken");
    expect(messages).toContain("InvalidProviderToken");
    expect(messages).toContain("Other");
    expect(JSON.stringify(messages)).not.toContain("private-provider-content");
    expect(JSON.stringify(messages)).not.toContain(device.token);
    expect(JSON.stringify(messages)).not.toContain(installationId);
  });

  it("reports missing or invalid signing configuration without a network request", async () => {
    await expect(new ApnsProvider({}).authorization()).rejects.toThrow("apns_not_configured");
    const { env } = await signingEnv();
    await expect(new ApnsProvider({ ...env, APNS_PRIVATE_KEY: "invalid" }).authorization()).rejects.toThrow("apns_invalid_configuration");
  });
});
