import { bearerFromRequest, credentialTransportAllowed, type Verified } from "./auth";
import {
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  SESSION_OWNER_AUTH_HEADER,
  type Env
} from "./env";
import { scopedSessionRoomKey } from "./room-key";

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const GRANT_TTL_MS = 15 * 60 * 1000;
const ACCESS_TTL_MS = 12 * 60 * 60 * 1000;

interface GrantRecord {
  grantId: string;
  ownerUserId: string;
  email: string;
  projectId: string;
  deploymentId: string;
  sandboxId: string;
  targetDeviceId: string;
  sessionId: string;
  capabilities: string[];
  grantHash: string;
  issuedAt: number;
  grantExpiresAt: number;
  consumedAt?: number;
  accessHash?: string;
  accessExpiresAt?: number;
  revokedAt?: number;
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" }
  });

const bodyJson = async <T>(request: Request | Response): Promise<T | undefined> => {
  try {
    return (await request.json()) as T;
  } catch {
    return undefined;
  }
};

const randomSecret = (): string =>
  `${crypto.randomUUID().replaceAll("-", "")}${crypto.randomUUID().replaceAll("-", "")}`;

const hash = async (value: string): Promise<string> => {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return btoa(String.fromCharCode(...new Uint8Array(digest)))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
};

export const parseGrantToken = (
  value: string,
  prefix: "cg1" | "cs1"
): { id: string; secret: string } | undefined => {
  const [actualPrefix, id, secret, extra] = value.split(".");
  if (actualPrefix !== prefix || extra !== undefined || !id || !secret) return undefined;
  if (!/^[a-f0-9]{32}$/.test(id) || !/^[a-f0-9]{64}$/.test(secret)) return undefined;
  return { id, secret };
};

const grantStub = (env: Env, id: string): DurableObjectStub =>
  env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(id));

/** One DO per grant keeps exchange atomic and makes unknown/revoked grants deny. */
export class AuthGrant implements DurableObject {
  constructor(private readonly ctx: DurableObjectState, _env: Env) {}

  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path === "/issue" && request.method === "POST") {
      const record = await bodyJson<GrantRecord>(request);
      if (!record || (await this.ctx.storage.get<GrantRecord>("grant"))) {
        return json({ error: "grant_conflict" }, 409);
      }
      await this.ctx.storage.put("grant", record);
      return json({ ok: true });
    }

    const record = await this.ctx.storage.get<GrantRecord>("grant");
    if (!record || record.revokedAt) return json({ error: "invalid_grant" }, 401);

    if (path === "/exchange" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        record.consumedAt ||
        record.grantExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.grantHash
      ) {
        return json({ error: "invalid_grant" }, 401);
      }
      const secret = randomSecret();
      record.consumedAt = Date.now();
      record.accessHash = await hash(secret);
      record.accessExpiresAt = Date.now() + ACCESS_TTL_MS;
      await this.ctx.storage.put("grant", record);
      return json({
        accessToken: `cs1.${record.grantId}.${secret}`,
        expiresAt: record.accessExpiresAt,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        capabilities: record.capabilities
      });
    }

    if (path === "/refresh" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        !record.accessHash ||
        !record.accessExpiresAt ||
        record.accessExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.accessHash
      ) {
        return json({ error: "invalid_token" }, 401);
      }
      const secret = randomSecret();
      record.accessHash = await hash(secret);
      record.accessExpiresAt = Date.now() + ACCESS_TTL_MS;
      await this.ctx.storage.put("grant", record);
      return json({
        accessToken: `cs1.${record.grantId}.${secret}`,
        expiresAt: record.accessExpiresAt,
        grantId: record.grantId,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        capabilities: record.capabilities
      });
    }

    if (path === "/authenticate" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        !record.accessHash ||
        !record.accessExpiresAt ||
        record.accessExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.accessHash
      ) {
        return json({ error: "invalid_token" }, 401);
      }
      return json({
        grantId: record.grantId,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        capabilities: record.capabilities,
        grantedAt: record.issuedAt,
        expiresAt: record.accessExpiresAt,
        revokedAt: null
      });
    }

    if (path === "/revoke" && request.method === "POST") {
      const body = await bodyJson<{ ownerUserId?: string }>(request);
      if (body?.ownerUserId !== record.ownerUserId) return json({ error: "forbidden" }, 403);
      record.revokedAt = Date.now();
      await this.ctx.storage.put("grant", record);
      return json({ ok: true });
    }
    return json({ error: "not_found" }, 404);
  }
}

export const authenticateDeviceToken = async (
  env: Env,
  token: string
): Promise<(Verified & {
  grantId: string;
  projectId: string;
  deploymentId: string;
  sandboxId: string;
  targetDeviceId: string;
  sessionId: string;
  grantedAt: number;
  expiresAt: number;
  revokedAt: null;
}) | undefined> => {
  const parsed = parseGrantToken(token, "cs1");
  if (!parsed) return undefined;
  const response = await grantStub(env, parsed.id).fetch("https://grant.internal/authenticate", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
  if (!response.ok) return undefined;
  const body = await bodyJson<{
    userId?: unknown;
    email?: unknown;
    grantId?: unknown;
    projectId?: unknown;
    deploymentId?: unknown;
    sandboxId?: unknown;
    targetDeviceId?: unknown;
    sessionId?: unknown;
    capabilities?: unknown;
    grantedAt?: unknown;
    expiresAt?: unknown;
    revokedAt?: unknown;
  }>(response);
  if (
    typeof body?.userId !== "string" ||
    typeof body.email !== "string" ||
    typeof body.projectId !== "string" ||
    body.projectId !== env.SCAFFOLD_PROJECT_SCOPE ||
    typeof body.deploymentId !== "string" ||
    typeof body.sandboxId !== "string" ||
    typeof body.grantId !== "string" ||
    typeof body.targetDeviceId !== "string" ||
    typeof body.sessionId !== "string" ||
    typeof body.grantedAt !== "number" ||
    !Number.isSafeInteger(body.grantedAt) ||
    typeof body.expiresAt !== "number" ||
    !Number.isSafeInteger(body.expiresAt) ||
    body.expiresAt <= Date.now() ||
    body.revokedAt !== null ||
    !Array.isArray(body.capabilities) ||
    !body.capabilities.every((value): value is string => typeof value === "string")
  ) {
    return undefined;
  }
  return {
    userId: body.userId,
    email: body.email,
    projectScope: body.projectId,
    grantId: body.grantId,
    projectId: body.projectId,
    deploymentId: body.deploymentId,
    sandboxId: body.sandboxId,
    targetDeviceId: body.targetDeviceId,
    sessionId: body.sessionId,
    grantedAt: body.grantedAt,
    expiresAt: body.expiresAt,
    revokedAt: null,
    capabilities: body.capabilities,
    credential: "device"
  };
};

/** Public one-time exchange. No other auth route is allowed before authentication. */
export const handlePublicAuthRoute = async (
  request: Request,
  env: Env,
  url: URL
): Promise<Response | undefined> => {
  if (url.pathname !== "/auth/device-grants/exchange") return undefined;
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
  const body = await bodyJson<{ grant?: string }>(request);
  const parsed = typeof body?.grant === "string" ? parseGrantToken(body.grant, "cg1") : undefined;
  if (!parsed) return json({ error: "invalid_grant" }, 401);
  return grantStub(env, parsed.id).fetch("https://grant.internal/exchange", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
};

interface SandboxTarget {
  projectId: string;
  sandboxId: string;
  deploymentId: string;
  targetDeviceId: string;
  sessionId: string;
}

const verifiedSandboxTarget = async (
  request: Request,
  env: Env,
  identity: Verified,
  requested: SandboxTarget
): Promise<SandboxTarget | undefined> => {
  const bearer = bearerFromRequest(request);
  if (!bearer || identity.credential !== "scaffold") return undefined;
  let origin: string;
  try {
    const configured = new URL(env.SCAFFOLD_CONTROL_PLANE_URL);
    if (
      !credentialTransportAllowed(configured) ||
      !["http:", "https:"].includes(configured.protocol) ||
      !["", "/"].includes(configured.pathname) ||
      configured.search ||
      configured.hash
    ) {
      return undefined;
    }
    origin = configured.origin;
  } catch {
    return undefined;
  }
  let response: Response;
  try {
    response = await fetch(
      `${origin}/api/code-sandboxes/${encodeURIComponent(requested.sandboxId)}/comet-target/verify`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${bearer}`,
          accept: "application/json",
          "content-type": "application/json"
        },
        body: JSON.stringify(requested),
        redirect: "manual"
      }
    );
  } catch {
    return undefined;
  }
  if (!response.ok) return undefined;
  const payload = await bodyJson<{
    ok?: unknown;
    profile?: {
      version?: unknown;
      projectId?: unknown;
      sandboxId?: unknown;
      deploymentId?: unknown;
      targetDeviceId?: unknown;
      sessionId?: unknown;
      actor?: { sub?: unknown };
    };
  }>(response);
  const profile = payload?.profile;
  const actorSubject =
    typeof profile?.actor?.sub === "string" ? profile.actor.sub.trim().toLowerCase() : "";
  if (
    payload?.ok !== true ||
    profile?.version !== "scaffold.comet-runtime.v1" ||
    actorSubject !== identity.userId ||
    typeof profile.projectId !== "string" ||
    profile.projectId !== requested.projectId ||
    profile.projectId !== identity.projectScope ||
    typeof profile.sandboxId !== "string" ||
    !ID_RE.test(profile.sandboxId) ||
    profile.sandboxId !== requested.sandboxId ||
    typeof profile.deploymentId !== "string" ||
    !ID_RE.test(profile.deploymentId) ||
    profile.deploymentId !== requested.deploymentId ||
    typeof profile.targetDeviceId !== "string" ||
    !ID_RE.test(profile.targetDeviceId) ||
    profile.targetDeviceId !== requested.targetDeviceId ||
    typeof profile.sessionId !== "string" ||
    !ID_RE.test(profile.sessionId) ||
    profile.sessionId !== requested.sessionId
  ) {
    return undefined;
  }
  return {
    projectId: profile.projectId,
    sandboxId: profile.sandboxId,
    deploymentId: profile.deploymentId,
    targetDeviceId: profile.targetDeviceId,
    sessionId: profile.sessionId
  };
};

const requesterOwnsSession = async (
  env: Env,
  target: Pick<SandboxTarget, "projectId" | "deploymentId" | "sessionId">,
  userId: string
): Promise<boolean> => {
  try {
    const roomName = scopedSessionRoomKey(target.projectId, target.deploymentId, target.sessionId);
    const room = env.SESSION_ROOMS.get(env.SESSION_ROOMS.idFromName(roomName));
    const response = await room.fetch(
      new Request("https://session.internal/authorize-owner", {
        method: "GET",
        headers: {
          [AUTH_USER_HEADER]: userId,
          [AUTH_PROJECT_HEADER]: target.projectId,
          [SESSION_OWNER_AUTH_HEADER]: "verify"
        }
      })
    );
    if (!response.ok) return false;
    const body = await bodyJson<{ ownsSession?: unknown }>(response);
    return body?.ownsSession === true;
  } catch {
    return false;
  }
};

export const handleAuthenticatedAuthRoute = async (
  request: Request,
  env: Env,
  url: URL,
  identity: Verified
): Promise<Response | undefined> => {
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (url.pathname === "/auth/device-token/refresh") {
    if (identity.credential !== "device") return json({ error: "forbidden" }, 403);
    if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const bearer = bearerFromRequest(request);
    const parsed = bearer ? parseGrantToken(bearer, "cs1") : undefined;
    const identityGrantId = "grantId" in identity ? identity.grantId : undefined;
    if (!parsed || typeof identityGrantId !== "string" || parsed.id !== identityGrantId) {
      return json({ error: "forbidden" }, 403);
    }
    return grantStub(env, parsed.id).fetch("https://grant.internal/refresh", {
      method: "POST",
      body: JSON.stringify({ secret: parsed.secret })
    });
  }
  if (url.pathname !== "/auth/device-grants") return undefined;
  if (identity.credential === "device") return json({ error: "forbidden" }, 403);

  if (request.method === "POST") {
    const body = await bodyJson<{
      deploymentId?: string;
      sandboxId?: string;
      targetDeviceId?: string;
      sessionId?: string;
      capabilities?: string[];
      ttlSeconds?: number;
    }>(request);
    if (
      !body ||
      typeof body.deploymentId !== "string" ||
      !ID_RE.test(body.deploymentId) ||
      typeof body.sandboxId !== "string" ||
      !ID_RE.test(body.sandboxId) ||
      typeof body.targetDeviceId !== "string" ||
      !ID_RE.test(body.targetDeviceId) ||
      typeof body.sessionId !== "string" ||
      !ID_RE.test(body.sessionId) ||
      !Array.isArray(body.capabilities) ||
      (body.ttlSeconds !== undefined &&
        (typeof body.ttlSeconds !== "number" || !Number.isFinite(body.ttlSeconds))) ||
      body.capabilities.length === 0 ||
      !body.capabilities.every(
        (capability) => typeof capability === "string" && identity.capabilities.includes(capability)
      )
    ) {
      return json({ error: "invalid_grant_request" }, 400);
    }
    const target = await verifiedSandboxTarget(request, env, identity, {
      projectId: identity.projectScope,
      deploymentId: body.deploymentId,
      sandboxId: body.sandboxId,
      targetDeviceId: body.targetDeviceId,
      sessionId: body.sessionId
    });
    if (!target) return json({ error: "grant_target_forbidden" }, 403);
    if (!(await requesterOwnsSession(env, target, identity.userId))) {
      return json({ error: "grant_target_forbidden" }, 403);
    }
    const id = crypto.randomUUID().replaceAll("-", "");
    const secret = randomSecret();
    const ttl = Math.min(
      GRANT_TTL_MS,
      Math.max(60_000, Math.floor((body.ttlSeconds ?? GRANT_TTL_MS / 1000) * 1000))
    );
    const issuedAt = Date.now();
    const expiresAt = issuedAt + ttl;
    const record: GrantRecord = {
      grantId: id,
      ownerUserId: identity.userId,
      email: identity.email,
      projectId: identity.projectScope,
      deploymentId: target.deploymentId,
      sandboxId: target.sandboxId,
      targetDeviceId: target.targetDeviceId,
      sessionId: target.sessionId,
      capabilities: [...new Set(body.capabilities)],
      grantHash: await hash(secret),
      issuedAt,
      grantExpiresAt: expiresAt
    };
    const response = await grantStub(env, id).fetch("https://grant.internal/issue", {
      method: "POST",
      body: JSON.stringify(record)
    });
    if (!response.ok) return json({ error: "grant_store_unavailable" }, 503);
    return json({
      grant: `cg1.${id}.${secret}`,
      expiresAt,
      accessExpiresAt: issuedAt + ACCESS_TTL_MS
    });
  }

  if (request.method === "DELETE") {
    const grantId = url.searchParams.get("id") ?? "";
    if (!/^[a-f0-9]{32}$/.test(grantId)) return json({ error: "invalid_grant_id" }, 400);
    return grantStub(env, grantId).fetch("https://grant.internal/revoke", {
      method: "POST",
      body: JSON.stringify({ ownerUserId: identity.userId })
    });
  }
  return json({ error: "method_not_allowed" }, 405);
};
