/**
 * UserDirectory — ONE Durable Object (`users1`) recording every WorkOS user:
 * name, email, profile picture and when they joined (WorkOS `created_at`).
 *
 * Fed by WorkOS webhooks (`user.created`, `user.updated`, `user.deleted`) at
 * `POST /workos/webhook`, signed with the endpoint's secret
 * (`WORKOS_WEBHOOK_SECRET`). Deliveries can arrive out of order and retry,
 * so a write lands only when its `updated_at` is at least the stored one;
 * a delete leaves a tombstone (id, joined, deleted) with the profile cleared,
 * and nothing revives it.
 *
 * `GET /admin/users[?format=csv]` (ADMIN_TOKEN) reads the table back.
 *
 * Writes are one row per sign-up or profile change, so a single instance is
 * not a hotspot.
 */
import { DurableObject } from "cloudflare:workers";
import type { Env } from "./env";
import { json, jsonError } from "./http";
import { adminAuthorized } from "./cloud/billing";

export const USER_DIRECTORY_NAME = "users1";

/** The fields of a WorkOS user object this directory keeps (wire names). */
export interface WorkOsUser {
  readonly id: string;
  readonly email: string;
  readonly first_name: string | null;
  readonly last_name: string | null;
  readonly profile_picture_url: string | null;
  readonly created_at: string;
  readonly updated_at: string;
}

export interface DirectoryUser {
  readonly userId: string;
  readonly email: string | null;
  readonly name: string | null;
  readonly firstName: string | null;
  readonly lastName: string | null;
  readonly profilePictureUrl: string | null;
  /** ISO 8601, WorkOS `created_at`. */
  readonly joinedAt: string;
  readonly updatedAt: string;
  readonly deletedAt: string | null;
}

export class UserDirectory extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.ctx.storage.sql.exec(`CREATE TABLE IF NOT EXISTS users (
      user_id TEXT PRIMARY KEY, email TEXT, first_name TEXT, last_name TEXT,
      profile_picture_url TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
      deleted_at TEXT, recorded_at INTEGER NOT NULL)`);
  }

  /** Record a user's current profile; stale or post-delete writes are no-ops.
   * Answers whether the row changed. */
  upsert(user: WorkOsUser): boolean {
    const cursor = this.ctx.storage.sql.exec(
      `INSERT INTO users (user_id, email, first_name, last_name, profile_picture_url, created_at, updated_at, recorded_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (user_id) DO UPDATE SET
         email = excluded.email, first_name = excluded.first_name, last_name = excluded.last_name,
         profile_picture_url = excluded.profile_picture_url, created_at = excluded.created_at,
         updated_at = excluded.updated_at, recorded_at = excluded.recorded_at
       WHERE users.deleted_at IS NULL AND excluded.updated_at >= users.updated_at`,
      user.id,
      user.email,
      user.first_name,
      user.last_name,
      user.profile_picture_url,
      user.created_at,
      user.updated_at,
      Date.now()
    );
    return cursor.rowsWritten > 0;
  }

  /** Tombstone a deleted user: keep id + joined date, drop the profile. */
  remove(user: WorkOsUser, deletedAt: string): void {
    this.ctx.storage.sql.exec(
      `INSERT INTO users (user_id, created_at, updated_at, deleted_at, recorded_at) VALUES (?, ?, ?, ?, ?)
       ON CONFLICT (user_id) DO UPDATE SET
         email = NULL, first_name = NULL, last_name = NULL, profile_picture_url = NULL,
         deleted_at = excluded.deleted_at, recorded_at = excluded.recorded_at`,
      user.id,
      user.created_at,
      user.updated_at,
      deletedAt,
      Date.now()
    );
  }

  list(): DirectoryUser[] {
    return [...this.ctx.storage.sql.exec("SELECT * FROM users ORDER BY created_at")].map((row) => {
      const first = row.first_name as string | null;
      const last = row.last_name as string | null;
      const name = [first, last].filter(Boolean).join(" ");
      return {
        userId: row.user_id as string,
        email: row.email as string | null,
        name: name || null,
        firstName: first,
        lastName: last,
        profilePictureUrl: row.profile_picture_url as string | null,
        joinedAt: row.created_at as string,
        updatedAt: row.updated_at as string,
        deletedAt: row.deleted_at as string | null
      };
    });
  }
}

export const userDirectoryStub = (env: Pick<Env, "USER_DIRECTORY">) =>
  env.USER_DIRECTORY ? env.USER_DIRECTORY.get(env.USER_DIRECTORY.idFromName(USER_DIRECTORY_NAME)) : undefined;

// ── webhook signature ───────────────────────────────────────────────────────

/** How far a webhook's signed timestamp may drift (the WorkOS SDK default). */
export const WORKOS_SIGNATURE_TOLERANCE_MS = 180_000;

const hex = (bytes: ArrayBuffer): string =>
  [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");

const timingSafeEqual = (a: string, b: string): boolean => {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
};

/** Sign `payload` the way WorkOS does: hex HMAC-SHA256 of `{ms}.{body}`. */
export const signWorkOsPayload = async (secret: string, timestampMs: number, payload: string): Promise<string> => {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  return hex(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(`${timestampMs}.${payload}`)));
};

/** `WorkOS-Signature: t={ms}, v1={hex}` over the raw body, within tolerance. */
export const verifyWorkOsSignature = async (
  secret: string,
  header: string | null,
  payload: string,
  nowMs = Date.now()
): Promise<boolean> => {
  if (!header) return false;
  let timestamp: number | undefined;
  let signature: string | undefined;
  for (const part of header.split(",")) {
    const [key, value] = part.trim().split("=", 2);
    if (key === "t" && value) timestamp = Number(value);
    if (key === "v1" && value) signature = value;
  }
  if (!timestamp || !Number.isFinite(timestamp) || !signature) return false;
  if (Math.abs(nowMs - timestamp) > WORKOS_SIGNATURE_TOLERANCE_MS) return false;
  return timingSafeEqual(signature, await signWorkOsPayload(secret, timestamp, payload));
};

// ── routes ──────────────────────────────────────────────────────────────────

/** WorkOS events are a few KB; anything larger isn't one. */
const MAX_WEBHOOK_BODY = 256 * 1024;

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const optionalString = (value: unknown): string | null => (typeof value === "string" && value ? value : null);

/** The user in a `user.*` event's `data`, or undefined if it isn't one. */
const parseUser = (data: unknown): WorkOsUser | undefined => {
  if (!isObject(data)) return undefined;
  const { id, email, created_at, updated_at } = data;
  if (typeof id !== "string" || !id.startsWith("user_")) return undefined;
  if (typeof email !== "string" || typeof created_at !== "string" || typeof updated_at !== "string") return undefined;
  return {
    id,
    email,
    first_name: optionalString(data.first_name),
    last_name: optionalString(data.last_name),
    profile_picture_url: optionalString(data.profile_picture_url),
    created_at,
    updated_at
  };
};

const csvField = (value: string | null): string =>
  value === null ? "" : /[",\n]/.test(value) ? `"${value.replace(/"/g, '""')}"` : value;

/**
 * `POST /workos/webhook` (WorkOS-signed, no bearer) and `GET /admin/users`
 * (ADMIN_TOKEN) — routed BEFORE the bearer gate. Events other than
 * `user.created|updated|deleted` are acknowledged so WorkOS stops retrying.
 */
export const handleUserDirectoryRoute = async (
  request: Request,
  env: Env,
  url: URL
): Promise<Response | undefined> => {
  if (url.pathname === "/admin/users") {
    if (!env.ADMIN_TOKEN) return json({ error: "not_found" }, 404);
    if (!(await adminAuthorized(request, env.ADMIN_TOKEN))) {
      return jsonError(401, "unauthenticated", "Admin token required.");
    }
    if (request.method !== "GET") return jsonError(405, "method_not_allowed", "GET only.");
    const directory = userDirectoryStub(env);
    if (!directory) return jsonError(503, "unavailable", "The user directory isn't set up on this server.");
    const users = await directory.list();
    if (url.searchParams.get("format") === "csv") {
      const header = "user_id,email,name,first_name,last_name,profile_picture_url,joined_at,updated_at,deleted_at";
      const rows = users.map((u) =>
        [u.userId, u.email, u.name, u.firstName, u.lastName, u.profilePictureUrl, u.joinedAt, u.updatedAt, u.deletedAt]
          .map(csvField)
          .join(",")
      );
      return new Response(`${[header, ...rows].join("\n")}\n`, {
        headers: {
          "content-type": "text/csv; charset=utf-8",
          "content-disposition": 'attachment; filename="zeron-users.csv"'
        }
      });
    }
    return json({ count: users.length, users });
  }

  if (url.pathname !== "/workos/webhook") return undefined;
  if (request.method !== "POST") return jsonError(405, "method_not_allowed", "POST only.");
  const directory = userDirectoryStub(env);
  if (!env.WORKOS_WEBHOOK_SECRET || !directory) {
    return jsonError(503, "unavailable", "WorkOS webhooks aren't set up on this server.");
  }
  const payload = await request.text();
  if (payload.length > MAX_WEBHOOK_BODY) return jsonError(413, "too_large", "Body too large.");
  if (!(await verifyWorkOsSignature(env.WORKOS_WEBHOOK_SECRET, request.headers.get("workos-signature"), payload))) {
    return jsonError(400, "bad_signature", "Signature check failed.");
  }
  let event: unknown;
  try {
    event = JSON.parse(payload);
  } catch {
    return jsonError(400, "bad_request", "Body must be JSON.");
  }
  if (!isObject(event) || typeof event.event !== "string") {
    return jsonError(400, "bad_request", "Body must be a WorkOS event.");
  }
  if (!["user.created", "user.updated", "user.deleted"].includes(event.event)) return json({ received: true });
  const user = parseUser(event.data);
  if (!user) return jsonError(400, "bad_request", "Event data must be a WorkOS user.");
  try {
    if (event.event === "user.deleted") {
      await directory.remove(user, typeof event.created_at === "string" ? event.created_at : new Date().toISOString());
      return json({ received: true, applied: true });
    }
    return json({ received: true, applied: await directory.upsert(user) });
  } catch (e) {
    console.error("users: applying a webhook failed", event.id, String(e));
    // WorkOS retries a non-2xx: nothing is lost.
    return jsonError(503, "unavailable", "Try again.");
  }
};
