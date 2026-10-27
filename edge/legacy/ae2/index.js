var __defProp = Object.defineProperty;
var __name = (target, value) => __defProp(target, "name", { value, configurable: true });
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// src/auth.ts
var REMOTE_CODE_CAPABILITIES = {
  "remote_code:create": ["session.invite"],
  "remote_code:read": ["session.read"],
  "remote_code:write": ["session.chat", "session.annotate", "session.files"],
  "remote_code:exec": ["session.control", "session.environment"],
  "remote_code:lifecycle": ["session.control", "session.environment"]
};
var capabilitiesFromRemoteCodeScopes = /* @__PURE__ */ __name((scopes) => [
  ...new Set(
    scopes.flatMap(
      (scope) => Object.hasOwn(REMOTE_CODE_CAPABILITIES, scope) ? REMOTE_CODE_CAPABILITIES[scope] : []
    )
  )
], "capabilitiesFromRemoteCodeScopes");
var LOOPBACK_HOSTS = {
  localhost: true,
  "127.0.0.1": true,
  "::1": true,
  "[::1]": true
};
var credentialTransportAllowed = /* @__PURE__ */ __name((value) => {
  try {
    const url = typeof value === "string" ? new URL(value) : value;
    if (url.username || url.password) return false;
    if (url.protocol === "https:" || url.protocol === "wss:") return true;
    return LOOPBACK_HOSTS[url.hostname] === true && (url.protocol === "http:" || url.protocol === "ws:");
  } catch {
    return false;
  }
}, "credentialTransportAllowed");
var normalizedOrigin = /* @__PURE__ */ __name((value) => {
  try {
    const url = new URL(value);
    if (!credentialTransportAllowed(url) || !["http:", "https:"].includes(url.protocol) || !["", "/"].includes(url.pathname) || url.search || url.hash) {
      return void 0;
    }
    return url.origin;
  } catch {
    return void 0;
  }
}, "normalizedOrigin");
var requiredCapabilities = /* @__PURE__ */ __name((env) => [...new Set(env.SCAFFOLD_REQUIRED_CAPABILITIES.split(/\s+/).map((value) => value.trim()).filter(Boolean))], "requiredCapabilities");
var bearerFromRequest = /* @__PURE__ */ __name((request) => {
  const header = request.headers.get("authorization");
  if (header?.toLowerCase().startsWith("bearer ")) return header.slice(7).trim() || void 0;
  const url = new URL(request.url);
  return url.searchParams.get("token")?.trim() || void 0;
}, "bearerFromRequest");
var SCAFFOLD_AUTH_ATTEMPT_TIMEOUT_MS = 2918;
var verifyScaffoldTokenResult = /* @__PURE__ */ __name(async (env, token) => {
  if (!token.startsWith("sc_rc_")) return { status: "invalid" };
  const resource = normalizedOrigin(env.SCAFFOLD_CONTROL_PLANE_URL);
  const projectScope = env.SCAFFOLD_PROJECT_SCOPE.trim();
  const required = requiredCapabilities(env);
  if (!resource || !projectScope || required.length === 0) return { status: "unavailable" };
  let response;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      response = await fetch(`${resource}/api/code-sandboxes/auth/session`, {
        headers: { authorization: `Bearer ${token}`, accept: "application/json" },
        signal: AbortSignal.timeout(SCAFFOLD_AUTH_ATTEMPT_TIMEOUT_MS)
      });
      if (response.status !== 408 && response.status !== 429 && response.status < 500) break;
    } catch {
      response = void 0;
    }
  }
  if (!response) return { status: "unavailable" };
  if (response.status === 401 || response.status === 403) return { status: "invalid" };
  if (!response.ok) return { status: "unavailable" };
  let session;
  try {
    session = await response.json();
  } catch {
    return { status: "unavailable" };
  }
  if (!session || typeof session !== "object") return { status: "unavailable" };
  const subject = typeof session.actor?.sub === "string" ? session.actor.sub.trim().toLowerCase() : "";
  const scopes = Array.isArray(session.scopes) ? [...new Set(session.scopes.filter((scope) => typeof scope === "string" && scope.length > 0))] : [];
  const capabilities = capabilitiesFromRemoteCodeScopes(scopes);
  if (session.ok !== true || session.actor?.auth !== "iap" || !subject || normalizedOrigin(typeof session.resource === "string" ? session.resource : "") !== resource || !required.every((capability) => capabilities.includes(capability))) {
    return { status: "unavailable" };
  }
  return {
    status: "authenticated",
    identity: {
      userId: subject,
      email: subject,
      projectScope,
      capabilities,
      credential: "scaffold"
    }
  };
}, "verifyScaffoldTokenResult");
var authenticateScaffoldResult = /* @__PURE__ */ __name(async (env, request) => {
  if (!credentialTransportAllowed(request.url)) return { status: "invalid" };
  const token = bearerFromRequest(request);
  if (!token) return { status: "invalid" };
  if (env.AUTH_MODE === "dev") {
    if (env.ENVIRONMENT !== "local") return { status: "unavailable" };
    const [userId, requestedProject] = token.split("@", 2);
    if (!userId || requestedProject && requestedProject !== env.SCAFFOLD_PROJECT_SCOPE) return { status: "invalid" };
    const capabilities = requiredCapabilities(env);
    if (capabilities.length === 0) return { status: "unavailable" };
    return {
      status: "authenticated",
      identity: {
        userId,
        email: `${userId}@dev.local`,
        projectScope: env.SCAFFOLD_PROJECT_SCOPE,
        capabilities,
        credential: "dev"
      }
    };
  }
  return verifyScaffoldTokenResult(env, token);
}, "authenticateScaffoldResult");
var authenticateScaffold = /* @__PURE__ */ __name(async (env, request) => {
  const result = await authenticateScaffoldResult(env, request);
  return result.status === "authenticated" ? result.identity : void 0;
}, "authenticateScaffold");

// src/env.ts
var AUTH_USER_HEADER = "x-comet-auth-user";
var AUTH_PROJECT_HEADER = "x-comet-auth-project";
var AUTH_CAPABILITIES_HEADER = "x-comet-auth-capabilities";
var AUTH_GRANT_HEADER = "x-comet-auth-grant";
var ROOM_KIND_HEADER = "x-comet-room-kind";
var GRANT_EVENT_HEADER = "x-comet-internal-grant-event";
var DEVICE_HOST_AUTH_HEADER = "x-comet-internal-device-host-auth";
var SESSION_OWNER_AUTH_HEADER = "x-comet-internal-session-owner-auth";
var NOTIFICATION_BEARER_HEADER = "x-comet-notification-bearer";
var SYNC_PROTOCOL_QUERY = "syncProtocol";
var DURABLE_SYNC_PROTOCOL = "durable-records-v1";
var SYNC_PROTOCOL_HEADER = "x-comet-sync-protocol";
var CREW_UPDATE_REQUIRED = "Crew update required for durable sync";
var stripTrustedAuthHeaders = /* @__PURE__ */ __name((headers) => {
  headers.delete(AUTH_USER_HEADER);
  headers.delete(AUTH_PROJECT_HEADER);
  headers.delete(AUTH_CAPABILITIES_HEADER);
  headers.delete(AUTH_GRANT_HEADER);
  headers.delete(DEVICE_HOST_AUTH_HEADER);
  headers.delete(SESSION_OWNER_AUTH_HEADER);
  headers.delete(GRANT_EVENT_HEADER);
  headers.delete(ROOM_KIND_HEADER);
  headers.delete(NOTIFICATION_BEARER_HEADER);
  headers.delete(SYNC_PROTOCOL_HEADER);
}, "stripTrustedAuthHeaders");

// src/room-key.ts
var ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
var scopedSessionRoomKey = /* @__PURE__ */ __name((projectId, deploymentId, sessionId) => {
  if (![projectId, deploymentId, sessionId].every((value) => ID_RE.test(value))) {
    throw new Error("invalid_session_room_scope");
  }
  return `s4/${projectId}/${deploymentId}/${sessionId}`;
}, "scopedSessionRoomKey");

// src/auth-routes.ts
var ID_RE2 = /^[A-Za-z0-9_-]{1,128}$/;
var GRANT_TTL_MS = 15 * 60 * 1e3;
var ACCESS_TTL_MS = 12 * 60 * 60 * 1e3;
var SCAFFOLD_COMET_RUNTIME_VERSION = "scaffold.comet-runtime.v1";
var json = /* @__PURE__ */ __name((value, status = 200) => new Response(JSON.stringify(value), {
  status,
  headers: { "content-type": "application/json", "cache-control": "no-store" }
}), "json");
var bodyJson = /* @__PURE__ */ __name(async (request) => {
  try {
    return await request.json();
  } catch {
    return void 0;
  }
}, "bodyJson");
var randomSecret = /* @__PURE__ */ __name(() => `${crypto.randomUUID().replaceAll("-", "")}${crypto.randomUUID().replaceAll("-", "")}`, "randomSecret");
var hash = /* @__PURE__ */ __name(async (value) => {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return btoa(String.fromCharCode(...new Uint8Array(digest))).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", "");
}, "hash");
var parseGrantToken = /* @__PURE__ */ __name((value, prefix) => {
  const [actualPrefix, id, secret, extra] = value.split(".");
  if (actualPrefix !== prefix || extra !== void 0 || !id || !secret) return void 0;
  if (!/^[a-f0-9]{32}$/.test(id) || !/^[a-f0-9]{64}$/.test(secret)) return void 0;
  return { id, secret };
}, "parseGrantToken");
var grantStub = /* @__PURE__ */ __name((env, id) => env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(id)), "grantStub");
var AuthGrant = class {
  constructor(ctx, _env) {
    this.ctx = ctx;
  }
  ctx;
  static {
    __name(this, "AuthGrant");
  }
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/issue" && request.method === "POST") {
      const record2 = await bodyJson(request);
      if (!record2 || await this.ctx.storage.get("grant")) {
        return json({ error: "grant_conflict" }, 409);
      }
      await this.ctx.storage.put("grant", record2);
      return json({ ok: true });
    }
    const record = await this.ctx.storage.get("grant");
    if (!record || record.revokedAt) return json({ error: "invalid_grant" }, 401);
    if (path === "/exchange" && request.method === "POST") {
      const body = await bodyJson(request);
      if (typeof body?.secret !== "string" || record.consumedAt || record.grantExpiresAt <= Date.now() || await hash(body.secret) !== record.grantHash) {
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
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities
      });
    }
    if (path === "/refresh" && request.method === "POST") {
      const body = await bodyJson(request);
      if (typeof body?.secret !== "string" || !record.accessHash || !record.accessExpiresAt || record.accessExpiresAt <= Date.now() || await hash(body.secret) !== record.accessHash) {
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
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities
      });
    }
    if (path === "/authenticate" && request.method === "POST") {
      const body = await bodyJson(request);
      if (typeof body?.secret !== "string" || !record.accessHash || !record.accessExpiresAt || record.accessExpiresAt <= Date.now() || await hash(body.secret) !== record.accessHash) {
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
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities,
        grantedAt: record.issuedAt,
        expiresAt: record.accessExpiresAt,
        revokedAt: null
      });
    }
    if (path === "/revoke" && request.method === "POST") {
      const body = await bodyJson(request);
      if (body?.ownerUserId !== record.ownerUserId) return json({ error: "forbidden" }, 403);
      record.revokedAt = Date.now();
      await this.ctx.storage.put("grant", record);
      return json({ ok: true });
    }
    return json({ error: "not_found" }, 404);
  }
};
var authenticateDeviceToken = /* @__PURE__ */ __name(async (env, token) => {
  const parsed = parseGrantToken(token, "cs1");
  if (!parsed) return void 0;
  const response = await grantStub(env, parsed.id).fetch("https://grant.internal/authenticate", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
  if (!response.ok) return void 0;
  const body = await bodyJson(response);
  if (typeof body?.userId !== "string" || typeof body.email !== "string" || typeof body.projectId !== "string" || body.projectId !== env.SCAFFOLD_PROJECT_SCOPE || typeof body.deploymentId !== "string" || typeof body.sandboxId !== "string" || typeof body.grantId !== "string" || typeof body.targetDeviceId !== "string" || typeof body.sessionId !== "string" || typeof body.lifecycleEpoch !== "number" || !Number.isSafeInteger(body.lifecycleEpoch) || body.lifecycleEpoch < 1 || typeof body.grantedAt !== "number" || !Number.isSafeInteger(body.grantedAt) || typeof body.expiresAt !== "number" || !Number.isSafeInteger(body.expiresAt) || body.expiresAt <= Date.now() || body.revokedAt !== null || !Array.isArray(body.capabilities) || !body.capabilities.every((value) => typeof value === "string")) {
    return void 0;
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
    lifecycleEpoch: body.lifecycleEpoch,
    grantedAt: body.grantedAt,
    expiresAt: body.expiresAt,
    revokedAt: null,
    capabilities: body.capabilities,
    credential: "device"
  };
}, "authenticateDeviceToken");
var handlePublicAuthRoute = /* @__PURE__ */ __name(async (request, env, url) => {
  if (url.pathname !== "/auth/device-grants/exchange") return void 0;
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
  const body = await bodyJson(request);
  const parsed = typeof body?.grant === "string" ? parseGrantToken(body.grant, "cg1") : void 0;
  if (!parsed) return json({ error: "invalid_grant" }, 401);
  return grantStub(env, parsed.id).fetch("https://grant.internal/exchange", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
}, "handlePublicAuthRoute");
var verifiedSandboxTarget = /* @__PURE__ */ __name(async (request, env, identity, requested) => {
  const bearer = bearerFromRequest(request);
  if (!bearer || identity.credential !== "scaffold") return void 0;
  let origin;
  try {
    const configured = new URL(env.SCAFFOLD_CONTROL_PLANE_URL);
    if (!credentialTransportAllowed(configured) || !["http:", "https:"].includes(configured.protocol) || !["", "/"].includes(configured.pathname) || configured.search || configured.hash) {
      return void 0;
    }
    origin = configured.origin;
  } catch {
    return void 0;
  }
  let response;
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
    return void 0;
  }
  if (!response.ok) return void 0;
  const payload = await bodyJson(response);
  const profile = payload?.profile;
  const actorSubject = typeof profile?.actor?.sub === "string" ? profile.actor.sub.trim().toLowerCase() : "";
  if (payload?.ok !== true || profile?.version !== SCAFFOLD_COMET_RUNTIME_VERSION || actorSubject !== identity.userId || typeof profile.projectId !== "string" || profile.projectId !== requested.projectId || profile.projectId !== identity.projectScope || typeof profile.sandboxId !== "string" || !ID_RE2.test(profile.sandboxId) || profile.sandboxId !== requested.sandboxId || typeof profile.deploymentId !== "string" || !ID_RE2.test(profile.deploymentId) || profile.deploymentId !== requested.deploymentId || typeof profile.targetDeviceId !== "string" || !ID_RE2.test(profile.targetDeviceId) || typeof profile.lifecycleEpoch !== "number" || !Number.isSafeInteger(profile.lifecycleEpoch) || profile.lifecycleEpoch !== requested.lifecycleEpoch || profile.targetDeviceId !== requested.targetDeviceId || typeof profile.sessionId !== "string" || !ID_RE2.test(profile.sessionId) || profile.sessionId !== requested.sessionId) {
    return void 0;
  }
  return {
    projectId: profile.projectId,
    sandboxId: profile.sandboxId,
    deploymentId: profile.deploymentId,
    targetDeviceId: profile.targetDeviceId,
    lifecycleEpoch: profile.lifecycleEpoch,
    sessionId: profile.sessionId
  };
}, "verifiedSandboxTarget");
var requesterOwnsSession = /* @__PURE__ */ __name(async (env, target, userId) => {
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
    const body = await bodyJson(response);
    return body?.ownsSession === true;
  } catch {
    return false;
  }
}, "requesterOwnsSession");
var handleAuthenticatedAuthRoute = /* @__PURE__ */ __name(async (request, env, url, identity) => {
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (url.pathname === "/auth/device-token/refresh") {
    if (identity.credential !== "device") return json({ error: "forbidden" }, 403);
    if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const bearer = bearerFromRequest(request);
    const parsed = bearer ? parseGrantToken(bearer, "cs1") : void 0;
    const identityGrantId = "grantId" in identity ? identity.grantId : void 0;
    if (!parsed || typeof identityGrantId !== "string" || parsed.id !== identityGrantId) {
      return json({ error: "forbidden" }, 403);
    }
    return grantStub(env, parsed.id).fetch("https://grant.internal/refresh", {
      method: "POST",
      body: JSON.stringify({ secret: parsed.secret })
    });
  }
  if (url.pathname !== "/auth/device-grants") return void 0;
  if (identity.credential === "device") return json({ error: "forbidden" }, 403);
  if (request.method === "POST") {
    const body = await bodyJson(request);
    if (!body || typeof body.deploymentId !== "string" || !ID_RE2.test(body.deploymentId) || typeof body.sandboxId !== "string" || !ID_RE2.test(body.sandboxId) || typeof body.targetDeviceId !== "string" || !ID_RE2.test(body.targetDeviceId) || typeof body.sessionId !== "string" || typeof body.lifecycleEpoch !== "number" || !Number.isSafeInteger(body.lifecycleEpoch) || body.lifecycleEpoch < 1 || !ID_RE2.test(body.sessionId) || !Array.isArray(body.capabilities) || body.ttlSeconds !== void 0 && (typeof body.ttlSeconds !== "number" || !Number.isFinite(body.ttlSeconds)) || body.capabilities.length === 0 || !body.capabilities.every(
      (capability) => typeof capability === "string" && identity.capabilities.includes(capability)
    )) {
      return json({ error: "invalid_grant_request" }, 400);
    }
    const target = await verifiedSandboxTarget(request, env, identity, {
      projectId: identity.projectScope,
      deploymentId: body.deploymentId,
      sandboxId: body.sandboxId,
      targetDeviceId: body.targetDeviceId,
      sessionId: body.sessionId,
      lifecycleEpoch: body.lifecycleEpoch
    });
    if (!target) return json({ error: "grant_target_forbidden" }, 403);
    if (!await requesterOwnsSession(env, target, identity.userId)) {
      return json({ error: "grant_target_forbidden" }, 403);
    }
    const id = crypto.randomUUID().replaceAll("-", "");
    const secret = randomSecret();
    const ttl = Math.min(
      GRANT_TTL_MS,
      Math.max(6e4, Math.floor((body.ttlSeconds ?? GRANT_TTL_MS / 1e3) * 1e3))
    );
    const issuedAt = Date.now();
    const expiresAt = issuedAt + ttl;
    const record = {
      grantId: id,
      ownerUserId: identity.userId,
      email: identity.email,
      projectId: identity.projectScope,
      deploymentId: target.deploymentId,
      sandboxId: target.sandboxId,
      targetDeviceId: target.targetDeviceId,
      sessionId: target.sessionId,
      lifecycleEpoch: target.lifecycleEpoch,
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
}, "handleAuthenticatedAuthRoute");

// src/grant-authority.ts
var ID_RE3 = /^[A-Za-z0-9_-]{1,128}$/;
var hasGrantScope = /* @__PURE__ */ __name((value) => {
  if (!value || typeof value !== "object") return false;
  const record = value;
  return typeof record.grantId === "string" && ID_RE3.test(record.grantId) && typeof record.projectId === "string" && ID_RE3.test(record.projectId) && typeof record.deploymentId === "string" && ID_RE3.test(record.deploymentId) && typeof record.sessionId === "string" && ID_RE3.test(record.sessionId) && typeof record.targetDeviceId === "string" && ID_RE3.test(record.targetDeviceId) && Number.isSafeInteger(record.lifecycleEpoch) && record.lifecycleEpoch >= 1;
}, "hasGrantScope");
var isActiveGrant = /* @__PURE__ */ __name((value, expectedGrantId, now) => hasGrantScope(value) && value.grantId === expectedGrantId && Number.isSafeInteger(value.accessExpiresAt) && value.accessExpiresAt > now && value.revokedAt === void 0, "isActiveGrant");
var AuthGrant2 = class extends AuthGrant {
  constructor(authorityCtx, authorityEnv) {
    super(authorityCtx, authorityEnv);
    this.authorityCtx = authorityCtx;
    this.authorityEnv = authorityEnv;
  }
  authorityCtx;
  authorityEnv;
  static {
    __name(this, "AuthGrant");
  }
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/status") {
      if (request.headers.get(GRANT_EVENT_HEADER) !== "status") {
        return new Response(null, { status: 403 });
      }
      const record2 = await this.authorityCtx.storage.get("grant");
      const active = isActiveGrant(record2, url.searchParams.get("grantId"), Date.now());
      return new Response(null, { status: active ? 204 : 401 });
    }
    const isRevocation = request.method === "POST" && url.pathname === "/revoke";
    const stored = isRevocation ? await this.authorityCtx.storage.get("grant") : void 0;
    const record = hasGrantScope(stored) ? stored : void 0;
    const response = await super.fetch(request);
    if (isRevocation && response.ok && record) {
      const rooms = [
        {
          kind: "session",
          stub: this.authorityEnv.SESSION_ROOMS.get(
            this.authorityEnv.SESSION_ROOMS.idFromName(
              scopedSessionRoomKey(record.projectId, record.deploymentId, record.sessionId)
            )
          )
        },
        {
          kind: "workspace",
          stub: this.authorityEnv.SESSION_ROOMS.get(
            this.authorityEnv.SESSION_ROOMS.idFromName(`ws4/${record.projectId}`)
          )
        },
        {
          kind: "device",
          stub: this.authorityEnv.DEVICE_ROOMS.get(
            this.authorityEnv.DEVICE_ROOMS.idFromName(
              `d3/${record.projectId}/${record.targetDeviceId}`
            )
          )
        }
      ];
      await Promise.all(
        rooms.map(async ({ kind, stub }) => {
          try {
            const notification = await stub.fetch(
              new Request(`https://${kind}.internal/grant-revoked`, {
                method: "POST",
                headers: {
                  [GRANT_EVENT_HEADER]: "revoke",
                  "content-type": "application/json"
                },
                body: JSON.stringify({ grantId: record.grantId })
              })
            );
            if (!notification.ok) {
              throw new Error(`${kind} room returned ${notification.status}`);
            }
          } catch (error) {
            console.error("grant revocation delivery failed", kind, String(error));
          }
        })
      );
    }
    return response;
  }
};

// src/release-feed.ts
var encoder = new TextEncoder();
var TOKEN_ENDPOINT = "https://oauth2.googleapis.com/token";
var STORAGE_SCOPE = "https://www.googleapis.com/auth/devstorage.read_only";
var LATEST_ALIAS_RE = /^(?:(?:desktop(?:-staging)?|scaffold)-(?:manifest\.json|SHA256SUMS|latest\.txt)|manifest\.json|SHA256SUMS|install\.sh|latest\.txt)$/;
var base64Url = /* @__PURE__ */ __name((bytes) => {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}, "base64Url");
var privateKeyBytes = /* @__PURE__ */ __name((value) => {
  const base64 = value.trim().replaceAll("\\n", "\n").replace(/-----BEGIN PRIVATE KEY-----|-----END PRIVATE KEY-----|\s/g, "");
  if (!base64) throw new Error("release service account private key is unavailable");
  return Uint8Array.from(atob(base64), (character) => character.charCodeAt(0));
}, "privateKeyBytes");
var googleAccessToken = /* @__PURE__ */ __name(async (env) => {
  const email = env.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL?.trim();
  const privateKey = env.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY?.trim();
  if (!email || !privateKey) throw new Error("release service account is unavailable");
  const now = Math.floor(Date.now() / 1e3);
  const header = base64Url(encoder.encode(JSON.stringify({ alg: "RS256", typ: "JWT" })));
  const claims = base64Url(
    encoder.encode(
      JSON.stringify({
        iss: email,
        scope: STORAGE_SCOPE,
        aud: TOKEN_ENDPOINT,
        iat: now,
        exp: now + 3600
      })
    )
  );
  const unsigned = `${header}.${claims}`;
  const key = await crypto.subtle.importKey(
    "pkcs8",
    privateKeyBytes(privateKey),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const signature = new Uint8Array(
    await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, encoder.encode(unsigned))
  );
  const assertion = `${unsigned}.${base64Url(signature)}`;
  const response = await fetch(TOKEN_ENDPOINT, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
      assertion
    })
  });
  if (!response.ok) throw new Error(`release token exchange failed (${response.status})`);
  const body = await response.json();
  if (typeof body.access_token !== "string" || !body.access_token) {
    throw new Error("release token exchange returned no access token");
  }
  return body.access_token;
}, "googleAccessToken");
var errorResponse = /* @__PURE__ */ __name((error, status) => Response.json({ error }, { status, headers: { "cache-control": "private, no-store" } }), "errorResponse");
var fetchReleaseObject = /* @__PURE__ */ __name(async (env, file, method, accessToken = googleAccessToken) => {
  const bucket = env.COMET_RELEASES_GCS_BUCKET?.trim();
  if (!bucket) return errorResponse("release_feed_unavailable", 503);
  let token;
  try {
    token = await accessToken(env);
  } catch {
    return errorResponse("release_feed_unavailable", 503);
  }
  const object = encodeURIComponent(`releases/${file}`);
  const response = await fetch(
    `https://storage.googleapis.com/download/storage/v1/b/${encodeURIComponent(bucket)}/o/${object}?alt=media`,
    { method, headers: { authorization: `Bearer ${token}` }, redirect: "manual" }
  );
  if (response.status === 404) return errorResponse("not_found", 404);
  if (!response.ok) return errorResponse("release_fetch_failed", 502);
  const headers = new Headers();
  for (const name of ["content-type", "content-length", "content-disposition", "etag", "last-modified"]) {
    const value = response.headers.get(name);
    if (value) headers.set(name, value);
  }
  headers.set(
    "cache-control",
    LATEST_ALIAS_RE.test(file) ? "private, no-store" : "private, max-age=31536000, immutable"
  );
  return new Response(method === "HEAD" ? null : response.body, { status: 200, headers });
}, "fetchReleaseObject");

// src/session-room.ts
import { createHash } from "node:crypto";
import { isDeepStrictEqual } from "node:util";

// node_modules/loro-crdt/web/snippets/loro-wasm-704a44faa43c032c/inline0.js
var inline0_exports = {};
__export(inline0_exports, {
  stateBinary: () => stateBinary,
  stateCid: () => stateCid,
  stateContainer: () => stateContainer,
  stateIndex: () => stateIndex,
  stateOptions: () => stateOptions,
  stateRootCid: () => stateRootCid,
  stateSet: () => stateSet,
  stateSlice: () => stateSlice,
  stateValue: () => stateValue
});
var kinds = ["Map", "List", "MovableList", "Text", "Tree", "Counter"];
function stateOptions(options, document) {
  if (typeof options !== "object" || options === null || Array.isArray(options)) throw new Error("Invalid toContainerTree options");
  for (const key of Object.keys(options)) {
    if (key !== "text" && !(document && key === "roots")) throw new Error("Unknown toContainerTree option: " + key);
  }
  if (document && options.roots !== void 0 && !Array.isArray(options.roots)) throw new Error("roots must be an array");
  return options;
}
__name(stateOptions, "stateOptions");
function stateSlice(node, start, totalLength) {
  return { cid: node.cid, start, totalLength, items: node.value };
}
__name(stateSlice, "stateSlice");
function stateContainer(kind, cid, value) {
  return { type: kinds[kind], cid, value };
}
__name(stateContainer, "stateContainer");
function stateCid(peer, counter, kind) {
  return "cid:" + counter + "@" + peer + ":" + kinds[kind];
}
__name(stateCid, "stateCid");
function stateRootCid(name, kind) {
  return "cid:root-" + name + ":" + kinds[kind];
}
__name(stateRootCid, "stateRootCid");
function stateValue(value) {
  return { type: "Value", value };
}
__name(stateValue, "stateValue");
function stateBinary(bytes) {
  return bytes.slice();
}
__name(stateBinary, "stateBinary");
function stateIndex(array, index, value) {
  Object.defineProperty(array, index, { value, enumerable: true, writable: true, configurable: true });
}
__name(stateIndex, "stateIndex");
function stateSet(object, key, value) {
  Object.defineProperty(object, key, { value, enumerable: true, writable: true, configurable: true });
}
__name(stateSet, "stateSet");

// node_modules/loro-crdt/web/loro_wasm.js
var wasm;
var WASM_VECTOR_LEN = 0;
var cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
  if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
    cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
  }
  return cachedUint8ArrayMemory0;
}
__name(getUint8ArrayMemory0, "getUint8ArrayMemory0");
var cachedTextEncoder = typeof TextEncoder !== "undefined" ? new TextEncoder("utf-8") : { encode: /* @__PURE__ */ __name(() => {
  throw Error("TextEncoder not available");
}, "encode") };
var encodeString = typeof cachedTextEncoder.encodeInto === "function" ? function(arg, view) {
  return cachedTextEncoder.encodeInto(arg, view);
} : function(arg, view) {
  const buf = cachedTextEncoder.encode(arg);
  view.set(buf);
  return {
    read: arg.length,
    written: buf.length
  };
};
function passStringToWasm0(arg, malloc, realloc) {
  if (realloc === void 0) {
    const buf = cachedTextEncoder.encode(arg);
    const ptr2 = malloc(buf.length, 1) >>> 0;
    getUint8ArrayMemory0().subarray(ptr2, ptr2 + buf.length).set(buf);
    WASM_VECTOR_LEN = buf.length;
    return ptr2;
  }
  let len = arg.length;
  let ptr = malloc(len, 1) >>> 0;
  const mem = getUint8ArrayMemory0();
  let offset = 0;
  for (; offset < len; offset++) {
    const code = arg.charCodeAt(offset);
    if (code > 127) break;
    mem[ptr + offset] = code;
  }
  if (offset !== len) {
    if (offset !== 0) {
      arg = arg.slice(offset);
    }
    ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
    const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
    const ret = encodeString(arg, view);
    offset += ret.written;
    ptr = realloc(ptr, len, offset, 1) >>> 0;
  }
  WASM_VECTOR_LEN = offset;
  return ptr;
}
__name(passStringToWasm0, "passStringToWasm0");
var cachedDataViewMemory0 = null;
function getDataViewMemory0() {
  if (cachedDataViewMemory0 === null || cachedDataViewMemory0.buffer.detached === true || cachedDataViewMemory0.buffer.detached === void 0 && cachedDataViewMemory0.buffer !== wasm.memory.buffer) {
    cachedDataViewMemory0 = new DataView(wasm.memory.buffer);
  }
  return cachedDataViewMemory0;
}
__name(getDataViewMemory0, "getDataViewMemory0");
function addToExternrefTable0(obj) {
  const idx = wasm.__externref_table_alloc();
  wasm.__wbindgen_export_4.set(idx, obj);
  return idx;
}
__name(addToExternrefTable0, "addToExternrefTable0");
function handleError(f, args) {
  try {
    return f.apply(this, args);
  } catch (e) {
    const idx = addToExternrefTable0(e);
    wasm.__wbindgen_exn_store(idx);
  }
}
__name(handleError, "handleError");
var cachedTextDecoder = typeof TextDecoder !== "undefined" ? new TextDecoder("utf-8", { ignoreBOM: true, fatal: true }) : { decode: /* @__PURE__ */ __name(() => {
  throw Error("TextDecoder not available");
}, "decode") };
if (typeof TextDecoder !== "undefined") {
  cachedTextDecoder.decode();
}
function getStringFromWasm0(ptr, len) {
  ptr = ptr >>> 0;
  return cachedTextDecoder.decode(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}
__name(getStringFromWasm0, "getStringFromWasm0");
function getArrayU8FromWasm0(ptr, len) {
  ptr = ptr >>> 0;
  return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}
__name(getArrayU8FromWasm0, "getArrayU8FromWasm0");
function isLikeNone(x) {
  return x === void 0 || x === null;
}
__name(isLikeNone, "isLikeNone");
var CLOSURE_DTORS = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((state) => {
  wasm.__wbindgen_export_6.get(state.dtor)(state.a, state.b);
});
function makeMutClosure(arg0, arg1, dtor, f) {
  const state = { a: arg0, b: arg1, cnt: 1, dtor };
  const real = /* @__PURE__ */ __name((...args) => {
    state.cnt++;
    const a = state.a;
    state.a = 0;
    try {
      return f(a, state.b, ...args);
    } finally {
      if (--state.cnt === 0) {
        wasm.__wbindgen_export_6.get(state.dtor)(a, state.b);
        CLOSURE_DTORS.unregister(state);
      } else {
        state.a = a;
      }
    }
  }, "real");
  real.original = state;
  CLOSURE_DTORS.register(real, state, state);
  return real;
}
__name(makeMutClosure, "makeMutClosure");
function debugString(val) {
  const type = typeof val;
  if (type == "number" || type == "boolean" || val == null) {
    return `${val}`;
  }
  if (type == "string") {
    return `"${val}"`;
  }
  if (type == "symbol") {
    const description = val.description;
    if (description == null) {
      return "Symbol";
    } else {
      return `Symbol(${description})`;
    }
  }
  if (type == "function") {
    const name = val.name;
    if (typeof name == "string" && name.length > 0) {
      return `Function(${name})`;
    } else {
      return "Function";
    }
  }
  if (Array.isArray(val)) {
    const length = val.length;
    let debug = "[";
    if (length > 0) {
      debug += debugString(val[0]);
    }
    for (let i = 1; i < length; i++) {
      debug += ", " + debugString(val[i]);
    }
    debug += "]";
    return debug;
  }
  const builtInMatches = /\[object ([^\]]+)\]/.exec(toString.call(val));
  let className;
  if (builtInMatches && builtInMatches.length > 1) {
    className = builtInMatches[1];
  } else {
    return toString.call(val);
  }
  if (className == "Object") {
    try {
      return "Object(" + JSON.stringify(val) + ")";
    } catch (_) {
      return "Object";
    }
  }
  if (val instanceof Error) {
    return `${val.name}: ${val.message}
${val.stack}`;
  }
  return className;
}
__name(debugString, "debugString");
function passArray8ToWasm0(arg, malloc) {
  const ptr = malloc(arg.length * 1, 1) >>> 0;
  getUint8ArrayMemory0().set(arg, ptr / 1);
  WASM_VECTOR_LEN = arg.length;
  return ptr;
}
__name(passArray8ToWasm0, "passArray8ToWasm0");
function takeFromExternrefTable0(idx) {
  const value = wasm.__wbindgen_export_4.get(idx);
  wasm.__externref_table_dealloc(idx);
  return value;
}
__name(takeFromExternrefTable0, "takeFromExternrefTable0");
function decodeImportBlobMeta(blob, check_checksum) {
  const ptr0 = passArray8ToWasm0(blob, wasm.__wbindgen_malloc);
  const len0 = WASM_VECTOR_LEN;
  const ret = wasm.decodeImportBlobMeta(ptr0, len0, check_checksum);
  if (ret[2]) {
    throw takeFromExternrefTable0(ret[1]);
  }
  return takeFromExternrefTable0(ret[0]);
}
__name(decodeImportBlobMeta, "decodeImportBlobMeta");
function callPendingEvents() {
  wasm.callPendingEvents();
}
__name(callPendingEvents, "callPendingEvents");
function isValidContainerId(value) {
  const ptr0 = passStringToWasm0(value, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
  const len0 = WASM_VECTOR_LEN;
  const ret = wasm.isValidContainerId(ptr0, len0);
  return ret !== 0;
}
__name(isValidContainerId, "isValidContainerId");
function passArrayJsValueToWasm0(array, malloc) {
  const ptr = malloc(array.length * 4, 4) >>> 0;
  for (let i = 0; i < array.length; i++) {
    const add = addToExternrefTable0(array[i]);
    getDataViewMemory0().setUint32(ptr + 4 * i, add, true);
  }
  WASM_VECTOR_LEN = array.length;
  return ptr;
}
__name(passArrayJsValueToWasm0, "passArrayJsValueToWasm0");
function _assertClass(instance, klass) {
  if (!(instance instanceof klass)) {
    throw new Error(`expected instance of ${klass.name}`);
  }
}
__name(_assertClass, "_assertClass");
function getArrayJsValueFromWasm0(ptr, len) {
  ptr = ptr >>> 0;
  const mem = getDataViewMemory0();
  const result = [];
  for (let i = ptr; i < ptr + 4 * len; i += 4) {
    result.push(wasm.__wbindgen_export_4.get(mem.getUint32(i, true)));
  }
  wasm.__externref_drop_slice(ptr, len);
  return result;
}
__name(getArrayJsValueFromWasm0, "getArrayJsValueFromWasm0");
function __wbg_adapter_60(arg0, arg1, arg2) {
  wasm.closure159_externref_shim(arg0, arg1, arg2);
}
__name(__wbg_adapter_60, "__wbg_adapter_60");
function __wbg_adapter_63(arg0, arg1) {
  wasm._dyn_core_9b3796e30d99ddb7___ops__function__FnMut_____Output______as_wasm_bindgen_b305d52f4c18ec15___closure__WasmClosure___describe__invoke______(arg0, arg1);
}
__name(__wbg_adapter_63, "__wbg_adapter_63");
var AwarenessWasmFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_awarenesswasm_free(ptr >>> 0, 1));
var ChangeModifierFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_changemodifier_free(ptr >>> 0, 1));
var ChangeModifier = class _ChangeModifier {
  static {
    __name(this, "ChangeModifier");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_ChangeModifier.prototype);
    obj.__wbg_ptr = ptr;
    ChangeModifierFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    ChangeModifierFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_changemodifier_free(ptr, 0);
  }
  /**
   * @param {string} message
   * @returns {ChangeModifier}
   */
  setMessage(message) {
    const ptr0 = passStringToWasm0(message, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.changemodifier_setMessage(this.__wbg_ptr, ptr0, len0);
    return _ChangeModifier.__wrap(ret);
  }
  /**
   * @param {number} timestamp
   * @returns {ChangeModifier}
   */
  setTimestamp(timestamp) {
    const ret = wasm.changemodifier_setTimestamp(this.__wbg_ptr, timestamp);
    return _ChangeModifier.__wrap(ret);
  }
};
var CursorFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_cursor_free(ptr >>> 0, 1));
var Cursor = class _Cursor {
  static {
    __name(this, "Cursor");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_Cursor.prototype);
    obj.__wbg_ptr = ptr;
    CursorFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    CursorFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_cursor_free(ptr, 0);
  }
  /**
   * Get the id of the given container.
   * @returns {ContainerID}
   */
  containerId() {
    const ret = wasm.cursor_containerId(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the ID that represents the position.
   *
   * It can be undefined if it's not bind into a specific ID.
   * @returns {{ peer: PeerID, counter: number } | undefined}
   */
  pos() {
    const ret = wasm.cursor_pos(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * "Cursor"
   * @returns {any}
   */
  kind() {
    const ret = wasm.cursor_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get which side of the character/list item the cursor is on.
   * @returns {Side}
   */
  side() {
    const ret = wasm.cursor_side(this.__wbg_ptr);
    return ret;
  }
  /**
   * Decode the cursor from a Uint8Array.
   * @param {Uint8Array} data
   * @returns {Cursor}
   */
  static decode(data) {
    const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.cursor_decode(ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _Cursor.__wrap(ret[0]);
  }
  /**
   * Encode the cursor into a Uint8Array.
   * @returns {Uint8Array}
   */
  encode() {
    const ret = wasm.cursor_encode(this.__wbg_ptr);
    var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v1;
  }
};
var EphemeralStoreWasmFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_ephemeralstorewasm_free(ptr >>> 0, 1));
var EphemeralStoreWasm = class {
  static {
    __name(this, "EphemeralStoreWasm");
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    EphemeralStoreWasmFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_ephemeralstorewasm_free(ptr, 0);
  }
  /**
   * @returns {any}
   */
  getAllStates() {
    const ret = wasm.ephemeralstorewasm_getAllStates(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  removeOutdated() {
    wasm.ephemeralstorewasm_removeOutdated(this.__wbg_ptr);
  }
  /**
   * @param {Function} f
   * @returns {any}
   */
  subscribeLocalUpdates(f) {
    const ret = wasm.ephemeralstorewasm_subscribeLocalUpdates(this.__wbg_ptr, f);
    return ret;
  }
  /**
   * @param {string} key
   * @returns {any}
   */
  get(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.ephemeralstorewasm_get(this.__wbg_ptr, ptr0, len0);
    return ret;
  }
  /**
   * Creates a new `EphemeralStore` instance.
   *
   * The `timeout` parameter specifies the duration in milliseconds.
   * A state of a peer is considered outdated, if the last update of the state of the peer
   * is older than the `timeout`.
   * @param {number} timeout
   */
  constructor(timeout) {
    const ret = wasm.ephemeralstorewasm_new(timeout);
    this.__wbg_ptr = ret >>> 0;
    EphemeralStoreWasmFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * @param {string} key
   * @param {any} value
   */
  set(key, value) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    wasm.ephemeralstorewasm_set(this.__wbg_ptr, ptr0, len0, value);
  }
  /**
   * @returns {string[]}
   */
  keys() {
    const ret = wasm.ephemeralstorewasm_keys(this.__wbg_ptr);
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * @param {Uint8Array} data
   */
  apply(data) {
    const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.ephemeralstorewasm_apply(this.__wbg_ptr, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * @param {string} key
   */
  delete(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    wasm.ephemeralstorewasm_delete(this.__wbg_ptr, ptr0, len0);
  }
  /**
   * @param {string} key
   * @returns {Uint8Array}
   */
  encode(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.ephemeralstorewasm_encode(this.__wbg_ptr, ptr0, len0);
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
  }
  /**
   * If the state is empty.
   * @returns {boolean}
   */
  isEmpty() {
    const ret = wasm.ephemeralstorewasm_isEmpty(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * @returns {Uint8Array}
   */
  encodeAll() {
    const ret = wasm.ephemeralstorewasm_encodeAll(this.__wbg_ptr);
    var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v1;
  }
  /**
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.ephemeralstorewasm_subscribe(this.__wbg_ptr, f);
    return ret;
  }
};
var LoroCounterFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorocounter_free(ptr >>> 0, 1));
var LoroCounter = class _LoroCounter {
  static {
    __name(this, "LoroCounter");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroCounter.prototype);
    obj.__wbg_ptr = ptr;
    LoroCounterFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroCounterFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorocounter_free(ptr, 0);
  }
  /**
   * Whether the container is attached to a docuemnt.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorocounter_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that equals to this or created by this, otherwise `undefined`.
   * @returns {LoroTree | undefined}
   */
  getAttached() {
    const ret = wasm.lorocounter_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the value of the counter.
   * @returns {number}
   */
  getShallowValue() {
    const ret = wasm.lorocounter_getShallowValue(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0];
  }
  /**
   * The container id of this handler.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.lorocounter_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Create a new LoroCounter.
   */
  constructor() {
    const ret = wasm.lorocounter_new();
    this.__wbg_ptr = ret >>> 0;
    LoroCounterFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * "Counter"
   * @returns {'Counter'}
   */
  kind() {
    const ret = wasm.lorocounter_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the parent container of the counter container.
   *
   * - The parent container of the root counter is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.lorocounter_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * @returns {number}
   */
  toJSON() {
    const ret = wasm.lorocounter_toJSON(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0];
  }
  /**
   * Decrement the counter by the given value.
   * @param {number} value
   */
  decrement(value) {
    const ret = wasm.lorocounter_decrement(this.__wbg_ptr, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the value of the counter.
   * @returns {number}
   */
  get value() {
    const ret = wasm.lorocounter_value(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0];
  }
  /**
   * Increment the counter by the given value.
   * @param {number} value
   */
  increment(value) {
    const ret = wasm.lorocounter_increment(this.__wbg_ptr, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Subscribe to the changes of the counter.
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.lorocounter_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.lorocounter_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroDocFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorodoc_free(ptr >>> 0, 1));
var LoroDoc = class _LoroDoc {
  static {
    __name(this, "LoroDoc");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroDoc.prototype);
    obj.__wbg_ptr = ptr;
    LoroDocFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroDocFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorodoc_free(ptr, 0);
  }
  /**
   * Apply a batch of diff to the document
   *
   * A diff batch represents a set of changes between two versions of the document.
   * You can calculate a diff batch using `doc.diff()`.
   *
   * Changes are associated with container IDs. During diff application, if new containers were created in the source
   * document, they will be assigned fresh IDs in the target document. Loro automatically handles remapping these
   * container IDs from their original IDs to the new IDs as the diff is applied.
   *
   * @example
   * ```ts
   * const doc1 = new LoroDoc();
   * const doc2 = new LoroDoc();
   *
   * // Make some changes to doc1
   * const text = doc1.getText("text");
   * text.insert(0, "Hello");
   *
   * // Calculate diff between empty and current state
   * const diff = doc1.diff([], doc1.frontiers());
   *
   * // Apply changes to doc2
   * doc2.applyDiff(diff);
   * console.log(doc2.getText("text").toString()); // "Hello"
   * ```
   *
   * Pass `{ fullState: true }` when applying the result of `doc.diff()` to a document that may
   * keep hidden state for a mergeable child the diff makes visible again (e.g. applying
   * `doc.diff(afterDelete, beforeDelete)` to `doc` itself or to a copy of it). The child's
   * entry is then read as its full content and aligned with that hidden state. Without it
   * (the default), entries are applied as increments, which is right for events forwarded to
   * a document that shares the source's hidden state. Never pass it for events from local
   * commits; see `ApplyDiffOptions.fullState`.
   *
   * ```ts
   * const text = doc.getMap("m").ensureMergeableText("t");
   * text.insert(0, "hello");
   * const before = doc.frontiers();
   * doc.getMap("m").delete("t");
   * doc.applyDiff(doc.diff(doc.frontiers(), before), { fullState: true });
   * console.log(doc.getMap("m").get("t").toString()); // "hello"
   * ```
   * @param {[ContainerID, Diff|JsonDiff][]} diff
   * @param {ApplyDiffOptions | null} [options]
   */
  applyDiff(diff, options) {
    const ret = wasm.lorodoc_applyDiff(this.__wbg_ptr, diff, isLikeNone(options) ? 0 : addToExternrefTable0(options));
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Check if the doc contains the full history.
   * @returns {boolean}
   */
  isShallow() {
    const ret = wasm.lorodoc_isShallow(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the number of changes in the oplog.
   * @returns {number}
   */
  changeCount() {
    const ret = wasm.lorodoc_changeCount(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Get the value or container at the given path
   *
   * The path can be specified in different ways depending on the container type:
   *
   * For Tree:
   * 1. Using node IDs: `tree/{node_id}/property`
   * 2. Using indices: `tree/0/1/property`
   *
   * For List and MovableList:
   * - Using indices: `list/0` or `list/1/property`
   *
   * For Map:
   * - Using keys: `map/key` or `map/nested/property`
   *
   * For tree structures, index-based paths follow depth-first traversal order.
   * The indices start from 0 and represent the position of a node among its siblings.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("key", 1);
   * console.log(doc.getByPath("map/key")); // 1
   * console.log(doc.getByPath("map"));     // LoroMap
   * ```
   * @param {string} path
   * @returns {Value | Container | undefined}
   */
  getByPath(path) {
    const ptr0 = passStringToWasm0(path, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_getByPath(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get a LoroCounter by container id
   *
   * If the container does not exist, an error will be thrown.
   * @param {ContainerID | string} cid
   * @returns {LoroCounter}
   */
  getCounter(cid) {
    const ret = wasm.lorodoc_getCounter(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroCounter.__wrap(ret[0]);
  }
  /**
   * `detached` indicates that the `DocState` is not synchronized with the latest version of `OpLog`.
   *
   * > The document becomes detached during a `checkout` operation.
   * > Being `detached` implies that the `DocState` is not synchronized with the latest version of the `OpLog`.
   * > In a detached state, the document is not editable by default, and any `import` operations will be
   * > recorded in the `OpLog` without being applied to the `DocState`.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * const frontiers = doc.frontiers();
   * text.insert(0, "Hello World!");
   * console.log(doc.isDetached());  // false
   * doc.checkout(frontiers);
   * console.log(doc.isDetached());  // true
   * doc.attach();
   * console.log(doc.isDetached());  // false
   * ```
   * @returns {boolean}
   */
  isDetached() {
    const ret = wasm.lorodoc_isDetached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get peer id in decimal string.
   * @returns {PeerID}
   */
  get peerIdStr() {
    const ret = wasm.lorodoc_peerIdStr(this.__wbg_ptr);
    return ret;
  }
  /**
   * Set the peer ID of the current writer.
   *
   * It must be a number, a BigInt, or a decimal string that can be parsed to a unsigned 64-bit integer.
   *
   * Note: use it with caution. You need to make sure there is not chance that two peers
   * have the same peer ID. Otherwise, we cannot ensure the consistency of the document.
   * @param {number | bigint | `${number}`} peer_id
   */
  setPeerId(peer_id) {
    const ret = wasm.lorodoc_setPeerId(this.__wbg_ptr, peer_id);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the absolute position of the given Cursor
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "123");
   * const pos0 = text.getCursor(0, 0);
   * {
   *    const ans = doc.getCursorPos(pos0!);
   *    expect(ans.offset).toBe(0);
   * }
   * text.insert(0, "1");
   * {
   *    const ans = doc.getCursorPos(pos0!);
   *    expect(ans.offset).toBe(1);
   * }
   * ```
   * @param {Cursor} cursor
   * @returns {{ update?: Cursor, offset: number, side: Side } | undefined}
   */
  getCursorPos(cursor) {
    _assertClass(cursor, Cursor);
    const ret = wasm.lorodoc_getCursorPos(this.__wbg_ptr, cursor.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Check if the doc contains the target container.
   *
   * A root container always exists, while a normal container exists
   * if it has ever been created on the doc.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroMap, LoroText, LoroList } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const text = doc.getMap("map").setContainer("text", new LoroText());
   * const list = doc.getMap("map").setContainer("list", new LoroList());
   * expect(doc.isContainerExists("cid:root-map:Map")).toBe(true);
   * expect(doc.isContainerExists("cid:0@1:Text")).toBe(true);
   * expect(doc.isContainerExists("cid:1@1:List")).toBe(true);
   *
   * const doc2 = new LoroDoc();
   * // Containers exist, as long as the history or the doc state include it
   * doc.detach();
   * doc2.import(doc.export({ mode: "update" }));
   * expect(doc2.isContainerExists("cid:root-map:Map")).toBe(true);
   * expect(doc2.isContainerExists("cid:0@1:Text")).toBe(true);
   * expect(doc2.isContainerExists("cid:1@1:List")).toBe(true);
   * ```
   * @param {ContainerID} container_id
   * @returns {boolean}
   */
  hasContainer(container_id) {
    const ret = wasm.lorodoc_hasContainer(this.__wbg_ptr, container_id);
    return ret !== 0;
  }
  /**
   * Import a batch of updates or snapshots.
   *
   * It's more efficient than importing updates one by one.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * const updates = doc.export({ mode: "update" });
   * const snapshot = doc.export({ mode: "snapshot" });
   * const doc2 = new LoroDoc();
   * doc2.importBatch([snapshot, updates]);
   * ```
   * @param {Uint8Array[]} data
   * @returns {ImportStatus}
   */
  importBatch(data) {
    const ret = wasm.lorodoc_importBatch(this.__wbg_ptr, data);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Compare the ordering of two Frontiers.
   *
   * It's assumed that both Frontiers are included by the doc. Otherwise, an error will be thrown.
   *
   * Return value:
   *
   * - -1: a < b
   * - 0: a == b
   * - 1: a > b
   * - undefined: a ∥ b: a and b are concurrent
   * @param {({ peer: PeerID, counter: number })[]} a
   * @param {({ peer: PeerID, counter: number })[]} b
   * @returns {-1 | 1 | 0 | undefined}
   */
  cmpFrontiers(a, b) {
    const ptr0 = passArrayJsValueToWasm0(a, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passArrayJsValueToWasm0(b, wasm.__wbindgen_malloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_cmpFrontiers(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Debug the size of the history
   */
  debugHistory() {
    wasm.lorodoc_debugHistory(this.__wbg_ptr);
  }
  /**
   * Create a loro document from the snapshot.
   *
   * @see You can learn more [here](https://loro.dev/docs/tutorial/encoding).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt"
   *
   * const doc = new LoroDoc();
   * // ...
   * const bytes = doc.export({ mode: "snapshot" });
   * const loro = LoroDoc.fromSnapshot(bytes);
   * ```
   * @param {Uint8Array} snapshot
   * @returns {LoroDoc}
   */
  static fromSnapshot(snapshot) {
    const ptr0 = passArray8ToWasm0(snapshot, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_fromSnapshot(ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _LoroDoc.__wrap(ret[0]);
  }
  /**
   * Get the change that contains the specific ID
   * @param {{ peer: PeerID, counter: number }} id
   * @returns {Change}
   */
  getChangeAt(id) {
    const ret = wasm.lorodoc_getChangeAt(this.__wbg_ptr, id);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the version vector of the latest known version in OpLog.
   *
   * If you checkout to a specific version, this version vector will not change.
   * @returns {VersionVector}
   */
  oplogVersion() {
    const ret = wasm.lorodoc_oplogVersion(this.__wbg_ptr);
    return VersionVector.__wrap(ret);
  }
  /**
   * Get a LoroMovableList by container id
   *
   * The object returned is a new js object each time because it need to cross
   * the WASM boundary.
   *
   * If the container does not exist, an error will be thrown.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getMovableList("list");
   * ```
   * @param {ContainerID | string} cid
   * @returns {LoroMovableList}
   */
  getMovableList(cid) {
    const ret = wasm.lorodoc_getMovableList(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroMovableList.__wrap(ret[0]);
  }
  /**
   * Convert frontiers to a version vector
   *
   * Learn more about frontiers and version vector [here](https://loro.dev/docs/advanced/version_deep_dive)
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * const frontiers = doc.frontiers();
   * const version = doc.frontiersToVV(frontiers);
   * ```
   * @param {({ peer: PeerID, counter: number })[]} frontiers
   * @returns {VersionVector}
   */
  frontiersToVV(frontiers) {
    const ptr0 = passArrayJsValueToWasm0(frontiers, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_frontiersToVV(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return VersionVector.__wrap(ret[0]);
  }
  /**
   * Get all of changes in the oplog.
   *
   * Note: this method is expensive when the oplog is large. O(n)
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * const changes = doc.getAllChanges();
   *
   * for (let [peer, c] of changes.entries()){
   *     console.log("peer: ", peer);
   *     for (let change of c){
   *         console.log("change: ", change);
   *     }
   * }
   * ```
   * @returns {Map<PeerID, Change[]>}
   */
  getAllChanges() {
    const ret = wasm.lorodoc_getAllChanges(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the [frontiers](https://loro.dev/docs/advanced/version_deep_dive) of the latest version in OpLog.
   *
   * If you checkout to a specific version, this value will not change.
   * @returns {{ peer: PeerID, counter: number }[]}
   */
  oplogFrontiers() {
    const ret = wasm.lorodoc_oplogFrontiers(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert a version vector to frontiers
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * const version = doc.version();
   * const frontiers = doc.vvToFrontiers(version);
   * ```
   * @param {VersionVector} vv
   * @returns {{ peer: PeerID, counter: number }[]}
   */
  vvToFrontiers(vv) {
    _assertClass(vv, VersionVector);
    const ret = wasm.lorodoc_vvToFrontiers(this.__wbg_ptr, vv.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * The doc only contains the history since this version
   *
   * This is empty if the doc is not shallow.
   *
   * The ops included by the shallow history start version vector are not in the doc.
   * @returns {VersionVector}
   */
  shallowSinceVV() {
    const ret = wasm.lorodoc_shallowSinceVV(this.__wbg_ptr);
    return VersionVector.__wrap(ret);
  }
  /**
   * Set the rich text format configuration of the document.
   *
   * You need to config it if you use rich text `mark` method.
   * Specifically, you need to config the `expand` property of each style.
   *
   * Expand is used to specify the behavior of expanding when new text is inserted at the
   * beginning or end of the style.
   *
   * You can specify the `expand` option to set the behavior when inserting text at the boundary of the range.
   *
   * - `after`(default): when inserting text right after the given range, the mark will be expanded to include the inserted text
   * - `before`: when inserting text right before the given range, the mark will be expanded to include the inserted text
   * - `none`: the mark will not be expanded to include the inserted text at the boundaries
   * - `both`: when inserting text either right before or right after the given range, the mark will be expanded to include the inserted text
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * doc.configTextStyle({
   *   bold: { expand: "after" },
   *   link: { expand: "before" }
   * });
   * const text = doc.getText("text");
   * text.insert(0, "Hello World!");
   * text.mark({ start: 0, end: 5 }, "bold", true);
   * expect(text.toDelta()).toStrictEqual([
   *   {
   *     insert: "Hello",
   *     attributes: {
   *       bold: true,
   *     },
   *   },
   *   {
   *     insert: " World!",
   *   },
   * ] as Delta<string>[]);
   * ```
   * @param {{[key: string]: { expand: 'before'|'after'|'none'|'both' }}} styles
   */
  configTextStyle(styles) {
    const ret = wasm.lorodoc_configTextStyle(this.__wbg_ptr, styles);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get all ops of the change that contains the specific ID
   * @param {{ peer: PeerID, counter: number }} id
   * @returns {any[]}
   */
  getOpsInChange(id) {
    const ret = wasm.lorodoc_getOpsInChange(this.__wbg_ptr, id);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the shallow json format of the document state.
   *
   * Unlike `toJSON()` which recursively resolves all containers to their values,
   * `getShallowValue()` returns container IDs as strings for any nested containers.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * const tree = doc.getTree("tree");
   * const map = doc.getMap("map");
   * const shallowValue = doc.getShallowValue();
   * console.log(shallowValue);
   * // {
   * //   list: 'cid:root-list:List',
   * //   tree: 'cid:root-tree:Tree',
   * //   map: 'cid:root-map:Map'
   * // }
   *
   * // It points to the same container as `list`
   * const listB = doc.getContainerById(shallowValue.list);
   * ```
   * @returns {Record<string, ContainerID>}
   */
  getShallowValue() {
    const ret = wasm.lorodoc_getShallowValue(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Checkout the `DocState` to the latest version of `OpLog`.
   *
   * > The document becomes detached during a `checkout` operation.
   * > Being `detached` implies that the `DocState` is not synchronized with the latest version of the `OpLog`.
   * > In a detached state, the document is not editable by default, and any `import` operations will be
   * > recorded in the `OpLog` without being applied to the `DocState`.
   *
   * This has the same effect as `attach`.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * const frontiers = doc.frontiers();
   * text.insert(0, "Hello World!");
   * doc.checkout(frontiers);
   * // you need call `checkoutToLatest()` or `attach()` before changing the doc.
   * doc.checkoutToLatest();
   * text.insert(0, "Hi");
   * ```
   */
  checkoutToLatest() {
    const ret = wasm.lorodoc_checkoutToLatest(this.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Compare the version of the OpLog with the specified frontiers.
   *
   * This method is useful to compare the version by only a small amount of data.
   *
   * This method returns an integer indicating the relationship between the version of the OpLog (referred to as 'self')
   * and the provided 'frontiers' parameter:
   *
   * - -1: The version of 'self' is either less than 'frontiers' or is non-comparable (parallel) to 'frontiers',
   *        indicating that it is not definitively less than 'frontiers'.
   * - 0: The version of 'self' is equal to 'frontiers'.
   * - 1: The version of 'self' is greater than 'frontiers'.
   *
   * # Internal
   *
   * Frontiers cannot be compared without the history of the OpLog.
   * @param {({ peer: PeerID, counter: number })[]} frontiers
   * @returns {number}
   */
  cmpWithFrontiers(frontiers) {
    const ptr0 = passArrayJsValueToWasm0(frontiers, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_cmpWithFrontiers(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0];
  }
  /**
   * @param {{ peer: PeerID, counter: number, length: number }} idSpan
   * @returns {any}
   */
  exportJsonInIdSpan(idSpan) {
    const ret = wasm.lorodoc_exportJsonInIdSpan(this.__wbg_ptr, idSpan);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Subscribe to changes that may affect a JSONPath query.
   *
   * The callback receives no query result; it is a lightweight notifier and may
   * fire false positives so callers can debounce/throttle before running JSONPath
   * themselves.
   * @param {string} jsonpath
   * @param {Function} f
   * @returns {any}
   */
  subscribeJsonpath(jsonpath, f) {
    const ptr0 = passStringToWasm0(jsonpath, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_subscribeJsonpath(this.__wbg_ptr, ptr0, len0, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Delete all content from a root container and hide it from the document.
   *
   * When a root container is empty and hidden:
   * - It won't show up in `get_deep_value()` results
   * - It won't be included in document snapshots
   *
   * Only works on root containers (containers without parents).
   * @param {ContainerID} cid
   */
  deleteRootContainer(cid) {
    const ret = wasm.lorodoc_deleteRootContainer(this.__wbg_ptr, cid);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Export updates in the given range in JSON format.
   * @param {any} start_vv
   * @param {any} end_vv
   * @param {boolean | null} [with_peer_compression]
   * @returns {JsonSchema}
   */
  exportJsonUpdates(start_vv, end_vv, with_peer_compression) {
    const ret = wasm.lorodoc_exportJsonUpdates(this.__wbg_ptr, start_vv, end_vv, isLikeNone(with_peer_compression) ? 16777215 : with_peer_compression ? 1 : 0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the container corresponding to the container id.
   *
   * If the container does not exist, it returns `undefined`.
   *
   * A mergeable container (created via `ensureMergeableMap` and friends) is resolvable
   * right after it has been ensured — even before any op is written into it — for as
   * long as the parent map's child ref is alive. A mergeable container id that was
   * never ensured on this doc resolves to `undefined`.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * let text = doc.getText("text");
   * const textId = text.id;
   * text = doc.getContainerById(textId);
   * ```
   * @param {ContainerID} container_id
   * @returns {any}
   */
  getContainerById(container_id) {
    const ret = wasm.lorodoc_getContainerById(this.__wbg_ptr, container_id);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the number of operations in the pending transaction.
   *
   * The pending transaction is the one that is not committed yet. It will be committed
   * automatically after calling `doc.commit()`, `doc.export(mode)` or `doc.checkout(version)`.
   * @returns {number}
   */
  getPendingTxnLength() {
    const ret = wasm.lorodoc_getPendingTxnLength(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Import updates from the JSON format.
   *
   * only supports backward compatibility but not forward compatibility.
   * @param {string | JsonSchema} json
   * @returns {ImportStatus}
   */
  importJsonUpdates(json6) {
    const ret = wasm.lorodoc_importJsonUpdates(this.__wbg_ptr, json6);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Import a batch of updates and snapshots.
   *
   * It's more efficient than importing updates one by one.
   *
   * @deprecated Use `importBatch` instead.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * const updates = doc.export({ mode: "update" });
   * const snapshot = doc.export({ mode: "snapshot" });
   * const doc2 = new LoroDoc();
   * doc2.importBatch([snapshot, updates]);
   * ```
   * @param {Uint8Array[]} data
   * @returns {ImportStatus}
   */
  importUpdateBatch(data) {
    const ret = wasm.lorodoc_importUpdateBatch(this.__wbg_ptr, data);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Enables editing in detached mode, which is disabled by default.
   *
   * The doc enter detached mode after calling `detach` or checking out a non-latest version.
   *
   * # Important Notes:
   *
   * - This mode uses a different PeerID for each checkout.
   * - Ensure no concurrent operations share the same PeerID if set manually.
   * - Importing does not affect the document's state or version; changes are
   *   recorded in the [OpLog] only. Call `checkout` to apply changes.
   * @param {boolean} enable
   */
  setDetachedEditing(enable) {
    wasm.lorodoc_setDetachedEditing(this.__wbg_ptr, enable);
  }
  /**
   * Set whether to record the timestamp of each change. Default is `false`.
   *
   * If enabled, the Unix timestamp (in seconds) will be recorded for each change automatically.
   *
   * You can also set each timestamp manually when you commit a change.
   * The timestamp manually set will override the automatic one.
   *
   * NOTE: Timestamps are forced to be in ascending order in the OpLog's history.
   * If you commit a new change with a timestamp that is less than the existing one,
   * the largest existing timestamp will be used instead.
   * @param {boolean} auto_record
   */
  setRecordTimestamp(auto_record) {
    wasm.lorodoc_setRecordTimestamp(this.__wbg_ptr, auto_record);
  }
  /**
   * Subscribe to the pre-commit event.
   *
   * The callback will be called when the changes are committed but not yet applied to the OpLog.
   * You can modify the commit message and timestamp in the callback by `ChangeModifier`.
   * @param {Function} f
   * @returns {any}
   */
  subscribePreCommit(f) {
    const ret = wasm.lorodoc_subscribePreCommit(this.__wbg_ptr, f);
    return ret;
  }
  /**
   * Find the op id spans that between the `from` version and the `to` version.
   *
   * You can combine it with `exportJsonInIdSpan` to get the changes between two versions.
   *
   * You can use it to travel all the changes from `from` to `to`. `from` and `to` are frontiers,
   * and they can be concurrent to each other. You can use it to find all the changes related to an event:
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const docA = new LoroDoc();
   * docA.setPeerId("1");
   * const docB = new LoroDoc();
   *
   * docA.getText("text").update("Hello");
   * docA.commit();
   * const snapshot = docA.export({ mode: "snapshot" });
   * let done = false;
   * docB.subscribe(e => {
   *   const spans = docB.findIdSpansBetween(e.from, e.to);
   *   const changes = docB.exportJsonInIdSpan(spans.forward[0]);
   *   console.log(changes);
   *   // [{
   *   //   id: "0@1",
   *   //   timestamp: expect.any(Number),
   *   //   deps: [],
   *   //   lamport: 0,
   *   //   msg: undefined,
   *   //   ops: [{
   *   //     container: "cid:root-text:Text",
   *   //     counter: 0,
   *   //     content: {
   *   //       type: "insert",
   *   //       pos: 0,
   *   //       text: "Hello"
   *   //     }
   *   //   }]
   *   // }]
   * });
   * docB.import(snapshot);
   * ```
   * @param {({ peer: PeerID, counter: number })[]} from
   * @param {({ peer: PeerID, counter: number })[]} to
   * @returns {VersionVectorDiff}
   */
  findIdSpansBetween(from, to) {
    const ptr0 = passArrayJsValueToWasm0(from, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passArrayJsValueToWasm0(to, wasm.__wbindgen_malloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_findIdSpansBetween(this.__wbg_ptr, ptr0, len0, ptr1, len1);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the change of with specific peer_id and lamport <= given lamport
   * @param {string} peer_id
   * @param {number} lamport
   * @returns {Change | undefined}
   */
  getChangeAtLamport(peer_id, lamport) {
    const ptr0 = passStringToWasm0(peer_id, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_getChangeAtLamport(this.__wbg_ptr, ptr0, len0, lamport);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the path from the root to the container
   * @param {ContainerID} id
   * @returns {(string|number)[] | undefined}
   */
  getPathToContainer(id) {
    const ret = wasm.lorodoc_getPathToContainer(this.__wbg_ptr, id);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Gets container IDs modified in the given ID range.
   *
   * **NOTE:** This method will implicitly commit.
   *
   * This method identifies which containers were affected by changes in a given range of operations.
   * It can be used together with `doc.travelChangeAncestors()` to analyze the history of changes
   * and determine which containers were modified by each change.
   *
   * @param id - The starting ID of the change range
   * @param len - The length of the change range to check
   * @returns An array of container IDs that were modified in the given range
   * @param {{ peer: PeerID, counter: number }} id
   * @param {number} len
   * @returns {ContainerID[]}
   */
  getChangedContainersIn(id, len) {
    const ret = wasm.lorodoc_getChangedContainersIn(this.__wbg_ptr, id, len);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get deep value of the document with container id
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.lorodoc_getDeepValueWithID(this.__wbg_ptr);
    return ret;
  }
  /**
   * Set the origin of the next commit
   * @param {string} origin
   */
  setNextCommitOrigin(origin) {
    const ptr0 = passStringToWasm0(origin, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    wasm.lorodoc_setNextCommitOrigin(this.__wbg_ptr, ptr0, len0);
  }
  /**
   * Get the pending operations from the current transaction in JSON format
   *
   * This method returns a JSON representation of operations that have been applied
   * but not yet committed in the current transaction.
   *
   * It will use the same data format as `doc.exportJsonUpdates()`
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * // Get pending ops before commit
   * const pendingOps = doc.getPendingOpsFromCurrentTxnAsJson();
   * doc.commit();
   * const emptyOps = doc.getPendingOpsFromCurrentTxnAsJson(); // this is undefined
   * ```
   * @returns {JsonSchema | undefined}
   */
  getUncommittedOpsAsJson() {
    const ret = wasm.lorodoc_getUncommittedOpsAsJson(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Set the commit message of the next commit
   * @param {string} msg
   */
  setNextCommitMessage(msg) {
    const ptr0 = passStringToWasm0(msg, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    wasm.lorodoc_setNextCommitMessage(this.__wbg_ptr, ptr0, len0);
  }
  /**
   * Set the options of the next commit
   * @param {{ origin?: string, timestamp?: number, message?: string }} options
   */
  setNextCommitOptions(options) {
    const ret = wasm.lorodoc_setNextCommitOptions(this.__wbg_ptr, options);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * The doc only contains the history since this version
   *
   * This is empty if the doc is not shallow.
   *
   * The ops included by the shallow history start frontiers are not in the doc.
   * @returns {{ peer: PeerID, counter: number }[]}
   */
  shallowSinceFrontiers() {
    const ret = wasm.lorodoc_shallowSinceFrontiers(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Subscribe the updates from local edits
   * @param {Function} f
   * @returns {any}
   */
  subscribeLocalUpdates(f) {
    const ret = wasm.lorodoc_subscribeLocalUpdates(this.__wbg_ptr, f);
    return ret;
  }
  /**
   * Visit all the ancestors of the changes in causal order.
   *
   * @param ids - the changes to visit
   * @param f - the callback function, return `true` to continue visiting, return `false` to stop
   * @param {({ peer: PeerID, counter: number })[]} ids
   * @param {(change: Change) => boolean} f
   */
  travelChangeAncestors(ids, f) {
    const ptr0 = passArrayJsValueToWasm0(ids, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_travelChangeAncestors(this.__wbg_ptr, ptr0, len0, f);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Clear the options of the next commit
   */
  clearNextCommitOptions() {
    wasm.lorodoc_clearNextCommitOptions(this.__wbg_ptr);
  }
  /**
   * Configures the default text style for the document.
   *
   * This method sets the default text style configuration for the document when using LoroText.
   * If `None` is provided, the default style is reset.
   * @param {{ expand: 'before'|'after'|'none'|'both' } | undefined} style
   */
  configDefaultTextStyle(style) {
    const ret = wasm.lorodoc_configDefaultTextStyle(this.__wbg_ptr, style);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * If two continuous local changes are within (<=) the interval(**in seconds**), they will be merged into one change.
   *
   * The default value is 1_000 seconds.
   *
   * By default, we record timestamps in seconds for each change. So if the merge interval is 1, and changes A and B
   * have timestamps of 3 and 4 respectively, then they will be merged into one change
   * @param {number} interval
   */
  setChangeMergeInterval(interval) {
    wasm.lorodoc_setChangeMergeInterval(this.__wbg_ptr, interval);
  }
  /**
   * Set the timestamp of the next commit
   * @param {number} timestamp
   */
  setNextCommitTimestamp(timestamp) {
    wasm.lorodoc_setNextCommitTimestamp(this.__wbg_ptr, timestamp);
  }
  /**
   * Set whether to hide empty root containers.
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * console.log(doc.toJSON()); // { map: {} }
   * doc.setHideEmptyRootContainers(true);
   * console.log(doc.toJSON()); // {}
   * ```
   * @param {boolean} hide
   */
  setHideEmptyRootContainers(hide) {
    const ret = wasm.lorodoc_setHideEmptyRootContainers(this.__wbg_ptr, hide);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Whether the editing is enabled in detached mode.
   *
   * The doc enter detached mode after calling `detach` or checking out a non-latest version.
   *
   * # Important Notes:
   *
   * - This mode uses a different PeerID for each checkout.
   * - Ensure no concurrent operations share the same PeerID if set manually.
   * - Importing does not affect the document's state or version; changes are
   *   recorded in the [OpLog] only. Call `checkout` to apply changes.
   * @returns {boolean}
   */
  isDetachedEditingEnabled() {
    const ret = wasm.lorodoc_isDetachedEditingEnabled(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * @param {Function} f
   * @returns {any}
   */
  subscribeFirstCommitFromPeer(f) {
    const ret = wasm.lorodoc_subscribeFirstCommitFromPeer(this.__wbg_ptr, f);
    return ret;
  }
  /**
   * Create a new loro document.
   *
   * New document will have a random peer id.
   */
  constructor() {
    const ret = wasm.lorodoc_new();
    this.__wbg_ptr = ret >>> 0;
    LoroDocFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Calculate the differences between two frontiers
   *
   * The entries in the returned object are sorted by causal order: the creation of a child container will be
   * presented before its use.
   * @param {({ peer: PeerID, counter: number })[]} from
   * @param {({ peer: PeerID, counter: number })[]} to
   * @param {boolean | null} [for_json]
   * @returns {[ContainerID, Diff|JsonDiff][]}
   */
  diff(from, to, for_json) {
    const ptr0 = passArrayJsValueToWasm0(from, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passArrayJsValueToWasm0(to, wasm.__wbindgen_malloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_diff(this.__wbg_ptr, ptr0, len0, ptr1, len1, isLikeNone(for_json) ? 16777215 : for_json ? 1 : 0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Duplicate the document with a different PeerID
   *
   * The time complexity and space complexity of this operation are both O(n),
   *
   * When called in detached mode, it will fork at the current state frontiers.
   * It will have the same effect as `forkAt(&self.frontiers())`.
   * @returns {LoroDoc}
   */
  fork() {
    const ret = wasm.lorodoc_fork(this.__wbg_ptr);
    return _LoroDoc.__wrap(ret);
  }
  /**
   * Attach the document state to the latest known version.
   *
   * > The document becomes detached during a `checkout` operation.
   * > Being `detached` implies that the `DocState` is not synchronized with the latest version of the `OpLog`.
   * > In a detached state, the document is not editable, and any `import` operations will be
   * > recorded in the `OpLog` without being applied to the `DocState`.
   *
   * This method has the same effect as invoking `checkoutToLatest`.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * const frontiers = doc.frontiers();
   * text.insert(0, "Hello World!");
   * doc.checkout(frontiers);
   * // you need call `attach()` or `checkoutToLatest()` before changing the doc.
   * doc.attach();
   * text.insert(0, "Hi");
   * ```
   */
  attach() {
    wasm.lorodoc_attach(this.__wbg_ptr);
  }
  /**
   * Commit the cumulative auto-committed transaction.
   *
   * You can specify the `origin`, `timestamp`, and `message` of the commit.
   *
   * - The `origin` is used to mark the event
   * - The `message` works like a git commit message, which will be recorded and synced to peers
   * - The `timestamp` is the number of seconds that have elapsed since 00:00:00 UTC on January 1, 1970.
   *   It defaults to `Date.now() / 1000` when timestamp recording is enabled
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * NOTE: Timestamps are forced to be in ascending order.
   * If you commit a new change with a timestamp that is less than the existing one,
   * the largest existing timestamp will be used instead.
   *
   * NOTE: The `origin` will not be persisted, but the `message` will.
   *
   * Behavior on empty commits:
   * - This method is an explicit commit. If the pending transaction is empty, any provided
   *   options (message/timestamp/origin) are swallowed and will not carry over to the next commit.
   * - Implicit commits triggered by `export`/`checkout` act as processing barriers. If the
   *   transaction is empty in those cases, `message`/`timestamp`/`origin` are preserved for the
   *   next commit.
   * @param {{ origin?: string, timestamp?: number, message?: string } | null} [options]
   */
  commit(options) {
    const ret = wasm.lorodoc_commit(this.__wbg_ptr, isLikeNone(options) ? 0 : addToExternrefTable0(options));
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Detach the document state from the latest known version.
   *
   * After detaching, all import operations will be recorded in the `OpLog` without being applied to the `DocState`.
   * When `detached`, the document is not editable.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.detach();
   * console.log(doc.isDetached());  // true
   * ```
   */
  detach() {
    wasm.lorodoc_detach(this.__wbg_ptr);
  }
  /**
   * Export the document based on the specified ExportMode.
   *
   * @param mode - The export mode to use. Can be one of:
   *   - `{ mode: "snapshot" }`: Export a full snapshot of the document.
   *   - `{ mode: "update", from?: VersionVector }`: Export updates from the given version vector.
   *     If `from` is not provided, it will export the whole history of the document.
   *   - `{ mode: "updates-in-range", spans: { id: ID, len: number }[] }`: Export updates within the specified ID spans.
   *   - `{ mode: "shallow-snapshot", frontiers: Frontiers }`: Export a garbage-collected snapshot up to the given frontiers.
   *
   * @returns A byte array containing the exported data.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * doc.getText("text").update("Hello World");
   *
   * // Export a full snapshot
   * const snapshotBytes = doc.export({ mode: "snapshot" });
   *
   * // Export updates from a specific version
   * const vv = doc.oplogVersion();
   * doc.getText("text").update("Hello Loro");
   * const updateBytes = doc.export({ mode: "update", from: vv });
   *
   * // Export a shallow snapshot that only includes the history since the frontiers
   * const shallowBytes = doc.export({ mode: "shallow-snapshot", frontiers: doc.oplogFrontiers() });
   *
   * // Export updates within specific ID spans
   * const spanBytes = doc.export({
   *   mode: "updates-in-range",
   *   spans: [{ id: { peer: "1", counter: 0 }, len: 10 }]
   * });
   * ```
   * @param {ExportMode} mode
   * @returns {Uint8Array}
   */
  export(mode) {
    const ret = wasm.lorodoc_export(this.__wbg_ptr, mode);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v1;
  }
  /**
   * Import snapshot or updates into current doc.
   *
   * Note:
   * - Updates within the current version will be ignored
   * - Updates with missing dependencies will be pending until the dependencies are received
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * // get all updates of the doc
   * const updates = doc.export({ mode: "update" });
   * const snapshot = doc.export({ mode: "snapshot" });
   * const doc2 = new LoroDoc();
   * // import snapshot
   * doc2.import(snapshot);
   * // or import updates
   * doc2.import(updates);
   * ```
   * @param {Uint8Array} update_or_snapshot
   * @returns {ImportStatus}
   */
  import(update_or_snapshot) {
    const ptr0 = passArray8ToWasm0(update_or_snapshot, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_import(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Creates a new LoroDoc at a specified version (Frontiers)
   *
   * The created doc will only contain the history before the specified frontiers.
   * @param {({ peer: PeerID, counter: number })[]} frontiers
   * @returns {LoroDoc}
   */
  forkAt(frontiers) {
    const ptr0 = passArrayJsValueToWasm0(frontiers, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_forkAt(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _LoroDoc.__wrap(ret[0]);
  }
  /**
   * Get a LoroMap by container id
   *
   * The object returned is a new js object each time because it need to cross
   * the WASM boundary.
   *
   * If the container does not exist, an error will be thrown.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * ```
   * @param {ContainerID | string} cid
   * @returns {LoroMap}
   */
  getMap(cid) {
    const ret = wasm.lorodoc_getMap(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroMap.__wrap(ret[0]);
  }
  /**
   * Get the number of ops in the oplog.
   * @returns {number}
   */
  opCount() {
    const ret = wasm.lorodoc_opCount(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Peer ID of the current writer.
   * @returns {bigint}
   */
  get peerId() {
    const ret = wasm.lorodoc_peerId(this.__wbg_ptr);
    return BigInt.asUintN(64, ret);
  }
  /**
   * Get the json format of the entire document state.
   *
   * Unlike `getShallowValue()` which returns container IDs as strings,
   * `toJSON()` recursively resolves all containers to their actual values.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText, LoroMap } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, "Hello");
   * const text = list.insertContainer(0, new LoroText());
   * text.insert(0, "Hello");
   * const map = list.insertContainer(1, new LoroMap());
   * map.set("foo", "bar");
   * console.log(doc.toJSON());
   * // {"list": ["Hello", {"foo": "bar"}]}
   * ```
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.lorodoc_toJSON(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the version vector of the current document state.
   *
   * If you checkout to a specific version, the version vector will change.
   * @returns {VersionVector}
   */
  version() {
    const ret = wasm.lorodoc_version(this.__wbg_ptr);
    return VersionVector.__wrap(ret);
  }
  /**
   * Checkout the `DocState` to a specific version.
   *
   * > The document becomes detached during a `checkout` operation.
   * > Being `detached` implies that the `DocState` is not synchronized with the latest version of the `OpLog`.
   * > In a detached state, the document is not editable, and any `import` operations will be
   * > recorded in the `OpLog` without being applied to the `DocState`.
   *
   * You should call `attach` to attach the `DocState` to the latest version of `OpLog`.
   *
   * @param frontiers - the specific frontiers
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * const frontiers = doc.frontiers();
   * text.insert(0, "Hello World!");
   * doc.checkout(frontiers);
   * console.log(doc.toJSON()); // {"text": ""}
   * ```
   * @param {({ peer: PeerID, counter: number })[]} frontiers
   */
  checkout(frontiers) {
    const ptr0 = passArrayJsValueToWasm0(frontiers, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_checkout(this.__wbg_ptr, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get a LoroList by container id
   *
   * The object returned is a new js object each time because it need to cross
   * the WASM boundary.
   *
   * If the container does not exist, an error will be thrown.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * ```
   * @param {ContainerID | string} cid
   * @returns {LoroList}
   */
  getList(cid) {
    const ret = wasm.lorodoc_getList(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroList.__wrap(ret[0]);
  }
  /**
   * Get a LoroText by container id.
   *
   * The object returned is a new js object each time because it need to cross
   * the WASM boundary.
   *
   * If the container does not exist, an error will be thrown.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * ```
   * @param {ContainerID | string} cid
   * @returns {LoroText}
   */
  getText(cid) {
    const ret = wasm.lorodoc_getText(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroText.__wrap(ret[0]);
  }
  /**
   * Get a LoroTree by container id
   *
   * The object returned is a new js object each time because it need to cross
   * the WASM boundary.
   *
   * If the container does not exist, an error will be thrown.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * ```
   * @param {ContainerID | string} cid
   * @returns {LoroTree}
   */
  getTree(cid) {
    const ret = wasm.lorodoc_getTree(this.__wbg_ptr, cid);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroTree.__wrap(ret[0]);
  }
  /**
   * Get the [frontiers](https://loro.dev/docs/advanced/version_deep_dive) of the current document state.
   *
   * If you checkout to a specific version, this value will change.
   * @returns {{ peer: PeerID, counter: number }[]}
   */
  frontiers() {
    const ret = wasm.lorodoc_frontiers(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Evaluate JSONPath against a LoroDoc
   * @param {string} jsonpath
   * @returns {Array<any>}
   */
  JSONPath(jsonpath) {
    const ptr0 = passStringToWasm0(jsonpath, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_JSONPath(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Revert the document to the given frontiers.
   *
   * The doc will not become detached when using this method. Instead, it will generate a series
   * of operations to revert the document to the given version.
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * doc.commit();
   * doc.revertTo([{ peer: "1", counter: 1 }]);
   * expect(doc.getText("text").toString()).toBe("He");
   * ```
   * @param {({ peer: PeerID, counter: number })[]} frontiers
   */
  revertTo(frontiers) {
    const ptr0 = passArrayJsValueToWasm0(frontiers, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorodoc_revertTo(this.__wbg_ptr, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Subscribe to the changes of the loro document. The function will be called when the
   * transaction is committed and after importing updates/snapshot from remote.
   *
   * Returns a subscription callback, which can be used to unsubscribe.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * const sub = doc.subscribe((event)=>{
   *     console.log(event);
   * });
   * text.insert(0, "Hello");
   * // the events will be emitted when `commit()` is called.
   * doc.commit();
   * // unsubscribe
   * sub();
   * ```
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.lorodoc_subscribe(this.__wbg_ptr, f);
    return ret;
  }
  /**
   * Convert visible roots to independent container trees without committing.
   * The text format applies recursively, including Tree metadata. Unknown or
   * hidden roots are omitted; roots: [] returns an empty object. Reads never
   * create roots. Binary values are owned Uint8Arrays. Not a CRDT export.
   * @param {DocumentContainerTreeOptions | null} [opts]
   * @returns {Record<string, ContainerNode>}
   */
  toContainerTree(opts) {
    const ret = wasm.lorodoc_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroListFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorolist_free(ptr >>> 0, 1));
var LoroList = class _LoroList {
  static {
    __name(this, "LoroList");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroList.prototype);
    obj.__wbg_ptr = ptr;
    LoroListFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroListFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorolist_free(ptr, 0);
  }
  /**
   * Whether the container is attached to a document.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorolist_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that equals to this or created by this, otherwise `undefined`.
   * @returns {LoroList | undefined}
   */
  getAttached() {
    const ret = wasm.lorolist_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * @param {Container} child
   * @returns {Container}
   */
  pushContainer(child) {
    const ret = wasm.lorolist_pushContainer(this.__wbg_ptr, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the deep value of the elements in the range `[start, end)`.
   *
   * Each child container in the range is recursively resolved to its deep
   * value. Negative bounds are clamped to 0 and out-of-range bounds are
   * clamped to the list length; an empty or inverted range returns `[]`.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getRangeValue(0, 2));  // [100, "Hello"]
   * ```
   * @param {number} start
   * @param {number} end
   * @returns {any}
   */
  getRangeValue(start, end) {
    const ret = wasm.lorolist_getRangeValue(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Insert a container at the index.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.toJSON());  // [100, "Hello"];
   * ```
   * @param {number} index
   * @param {Container} child
   * @returns {Container}
   */
  insertContainer(index, child) {
    const ret = wasm.lorolist_insertContainer(this.__wbg_ptr, index, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the shallow value of the list.
   *
   * Unlike `toJSON()` which recursively resolves all containers to their values,
   * `getShallowValue()` returns container IDs as strings for any nested containers.
   *
   * ```js
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const list = doc.getList("list");
   * list.insert(0, 1);
   * list.insert(1, "two");
   * const subList = list.insertContainer(2, new LoroList());
   * subList.insert(0, "sub");
   * list.getShallowValue(); // [1, "two", "cid:2@1:List"]
   * list.toJSON(); // [1, "two", ["sub"]]
   * ```
   * @returns {Value[]}
   */
  getShallowValue() {
    const ret = wasm.lorolist_getShallowValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the deep value of the list with its container id.
   *
   * The result is a `{ cid, value }` object, where `cid` is the container id
   * string (the same as `list.id`) and `value` is the deep value of the
   * list. Child containers inside `value` are recursively replaced by their
   * own `{ cid, value }` nodes. This is the same node shape that
   * `LoroDoc.getDeepValueWithID()` emits for every container.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getDeepValueWithID());
   * // { cid: "cid:root-list:List",
   * //   value: [100, { cid: "cid:0@1:Text", value: "Hello" }] }
   * ```
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.lorolist_getDeepValueWithID(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the deep value of the elements in the range `[start, end)`, with
   * container ids.
   *
   * Each child container in the range is recursively replaced by its own
   * `{ cid, value }` node. Negative bounds are clamped to 0 and
   * out-of-range bounds are clamped to the list length; an empty or
   * inverted range returns `[]`.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getRangeDeepValueWithID(0, 2));
   * // [100, { cid: "cid:0@1:Text", value: "Hello" }]
   * ```
   * @param {number} start
   * @param {number} end
   * @returns {any}
   */
  getRangeDeepValueWithID(start, end) {
    const ret = wasm.lorolist_getRangeDeepValueWithID(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the id of this container.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.lorolist_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the value at the index. If the value is a container, the corresponding handler will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * console.log(list.get(0));  // 100
   * console.log(list.get(1));  // undefined
   * ```
   * @param {number} index
   * @returns {Value | Container | undefined}
   */
  get(index) {
    const ret = wasm.lorolist_get(this.__wbg_ptr, index);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Create a new detached LoroList (not attached to any LoroDoc).
   *
   * The edits on a detached container will not be persisted.
   * To attach the container to the document, please insert it into an attached container.
   */
  constructor() {
    const ret = wasm.lorolist_new();
    this.__wbg_ptr = ret >>> 0;
    LoroListFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Pop a value from the end of the list.
   * @returns {Value | undefined}
   */
  pop() {
    const ret = wasm.lorolist_pop(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * "List"
   * @returns {'List'}
   */
  kind() {
    const ret = wasm.lorolist_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Push a value to the end of the list.
   * @param {Value} value
   */
  push(value) {
    const ret = wasm.lorolist_push(this.__wbg_ptr, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Delete all elements in the list.
   */
  clear() {
    const ret = wasm.lorolist_clear(this.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Delete elements from index to index + len.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.delete(0, 1);
   * console.log(list.value);  // []
   * ```
   * @param {number} index
   * @param {number} len
   */
  delete(index, len) {
    const ret = wasm.lorolist_delete(this.__wbg_ptr, index, len);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Insert a value at index.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * console.log(list.value);  // [100, "foo", true];
   * ```
   * @param {number} index
   * @param {Value} value
   */
  insert(index, value) {
    const ret = wasm.lorolist_insert(this.__wbg_ptr, index, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the length of list.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * console.log(list.length);  // 3
   * ```
   * @returns {number}
   */
  get length() {
    const ret = wasm.lorolist_length(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Get the parent container.
   *
   * - The parent container of the root tree is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.lorolist_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * @param {number} pos
   * @returns {{ peer: PeerID, counter: number } | undefined}
   */
  getIdAt(pos) {
    const ret = wasm.lorolist_getIdAt(this.__wbg_ptr, pos);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get elements of the list. If the type of a element is a container, it will be
   * resolved recursively.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.toJSON());  // [100, "Hello"];
   * ```
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.lorolist_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get elements of the list. If the value is a child container, the corresponding
   * `Container` will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * list.insertContainer(3, new LoroText());
   * console.log(list.value);  // [100, "foo", true, LoroText];
   * ```
   * @returns {(Value | Container)[]}
   */
  toArray() {
    const ret = wasm.lorolist_toArray(this.__wbg_ptr);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the cursor at the position.
   *
   * - The first argument is the position .
   * - The second argument is the side: `-1` for left, `0` for middle, `1` for right.
   * @param {number} pos
   * @param {Side} side
   * @returns {Cursor | undefined}
   */
  getCursor(pos, side) {
    const ret = wasm.lorolist_getCursor(this.__wbg_ptr, pos, side);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 0 ? void 0 : Cursor.__wrap(ret[0]);
  }
  /**
   * Check if the container is deleted
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.lorolist_isDeleted(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Subscribe to the changes of the list.
   *
   * Returns a subscription callback, which can be used to unsubscribe.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.subscribe((event)=>{
   *     console.log(event);
   * });
   * list.insert(0, 100);
   * doc.commit();
   * ```
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.lorolist_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.lorolist_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Read [start,end), clamped to list length. Inverted bounds return no items.
   * Returns actual start, totalLength and items; this is not a complete container.
   * Only selected descendants are traversed; parent shallow list access is O(N).
   * @param {number} start
   * @param {number} end
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerTreeSlice}
   */
  toContainerTreeSlice(start, end, opts) {
    const ret = wasm.lorolist_toContainerTreeSlice(this.__wbg_ptr, start, end, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroMapFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_loromap_free(ptr >>> 0, 1));
var LoroMap = class _LoroMap {
  static {
    __name(this, "LoroMap");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroMap.prototype);
    obj.__wbg_ptr = ptr;
    LoroMapFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroMapFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_loromap_free(ptr, 0);
  }
  /**
   * Whether the container is attached to a document.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorolist_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that equals to this or created by this, otherwise `undefined`.
   * @returns {LoroMap | undefined}
   */
  getAttached() {
    const ret = wasm.loromap_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the peer id of the last editor on the given entry
   * @param {string} key
   * @returns {PeerID | undefined}
   */
  getLastEditor(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_getLastEditor(this.__wbg_ptr, ptr0, len0);
    return ret;
  }
  /**
   * Set the key with a regular child container.
   *
   * The inserted child receives a regular op-created container id. Use this
   * for replacement semantics or when each creation should produce a distinct
   * child. Use `ensureMergeable*` when peers may lazily initialize the same
   * logical child under a map key.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText, LoroList } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * const text = map.setContainer("text", new LoroText());
   * const list = map.setContainer("list", new LoroList());
   * ```
   * @param {string} key
   * @param {Container} child
   * @returns {Container}
   */
  setContainer(key, child) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_setContainer(this.__wbg_ptr, ptr0, len0, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the shallow value of the map.
   *
   * Unlike `toJSON()` which recursively resolves all containers to their values,
   * `getShallowValue()` returns container IDs as strings for any nested containers.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const map = doc.getMap("map");
   * map.set("key", "value");
   * const subText = map.setContainer("text", new LoroText());
   * subText.insert(0, "Hello");
   *
   * // Get shallow value - nested containers are represented by their IDs
   * console.log(map.getShallowValue());
   * // Output: { key: "value", text: "cid:1@1:Text" }
   *
   * // Get full value with nested containers resolved by `toJSON()`
   * console.log(map.toJSON());
   * // Output: { key: "value", text: "Hello" }
   * ```
   * @returns {Record<string, Value>}
   */
  getShallowValue() {
    const ret = wasm.loromap_getShallowValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * Ensure a mergeable Map exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   *
   * Prefer to avoid very deep mergeable-map chains: mergeable cids encode
   * their flattened logical path, so cid size still grows with depth and
   * rides through every op/snapshot reference to it.
   * @param {string} key
   * @returns {LoroMap}
   */
  ensureMergeableMap(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableMap(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _LoroMap.__wrap(ret[0]);
  }
  /**
   * Ensure a mergeable List exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   * @param {string} key
   * @returns {LoroList}
   */
  ensureMergeableList(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableList(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroList.__wrap(ret[0]);
  }
  /**
   * Ensure a mergeable Text exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   * @param {string} key
   * @returns {LoroText}
   */
  ensureMergeableText(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableText(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroText.__wrap(ret[0]);
  }
  /**
   * Ensure a mergeable Tree exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   * @param {string} key
   * @returns {LoroTree}
   */
  ensureMergeableTree(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableTree(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroTree.__wrap(ret[0]);
  }
  /**
   * Get the deep value of the map with its container id.
   *
   * The result is a `{ cid, value }` object, where `cid` is the container id
   * string (the same as `map.id`) and `value` is the deep value of the map.
   * Child containers inside `value` are recursively replaced by their own
   * `{ cid, value }` nodes. This is the same node shape that
   * `LoroDoc.getDeepValueWithID()` emits for every container.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * const text = map.setContainer("text", new LoroText());
   * text.insert(0, "Hello");
   * console.log(map.getDeepValueWithID());
   * // { cid: "cid:root-map:Map",
   * //   value: { foo: "bar", text: { cid: "cid:0@1:Text", value: "Hello" } } }
   * ```
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.loromap_getDeepValueWithID(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get or create a regular child container at the given key.
   *
   * @deprecated Use `ensureMergeable*` for lazy map-key child creation. This
   * method creates regular op-id children, so concurrent first creation at
   * the same key can fork child state and leave one branch hidden by map
   * conflict resolution. Use `setContainer` for regular replacement/attach
   * semantics.
   *
   * If the key is empty or `null`, this creates a regular child container.
   * If the key already holds a child of the same type, it returns that child.
   * If the key holds a scalar or a child of another type, it throws.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * const text = map.getOrCreateContainer("text", new LoroText());
   * text.insert(0, "legacy regular child");
   * ```
   * @param {string} key
   * @param {Container} child
   * @returns {Container}
   */
  getOrCreateContainer(key, child) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_getOrCreateContainer(this.__wbg_ptr, ptr0, len0, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Ensure a mergeable Counter exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const counter = doc.getMap("state").ensureMergeableCounter("revision");
   * counter.increment(1);
   * ```
   * @param {string} key
   * @returns {LoroCounter}
   */
  ensureMergeableCounter(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableCounter(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroCounter.__wrap(ret[0]);
  }
  /**
   * Ensure a mergeable MovableList exists under the given key and return it.
   *
   * If the map is detached, or if the key already holds a non-mergeable
   * value, this throws and leaves the value unchanged.
   * @param {string} key
   * @returns {LoroMovableList}
   */
  ensureMergeableMovableList(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_ensureMergeableMovableList(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroMovableList.__wrap(ret[0]);
  }
  /**
   * The container id of this handler.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.loromap_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the value of the key. If the value is a child container, the corresponding
   * `Container` will be returned.
   *
   * The object/value returned is a new js object/value each time because it need to cross
   * the WASM boundary.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * const bar = map.get("foo");
   * ```
   * @param {string} key
   * @returns {Value | Container | undefined}
   */
  get(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_get(this.__wbg_ptr, ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Create a new detached LoroMap (not attached to any LoroDoc).
   *
   * The edits on a detached container will not be persisted.
   * To attach the container to the document, please insert it into an attached container.
   */
  constructor() {
    const ret = wasm.loromap_new();
    this.__wbg_ptr = ret >>> 0;
    LoroMapFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Get the keys of the map.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * map.set("baz", "bar");
   * const keys = map.keys(); // ["foo", "baz"]
   * ```
   * @returns {any[]}
   */
  keys() {
    const ret = wasm.loromap_keys(this.__wbg_ptr);
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * "Map"
   * @returns {'Map'}
   */
  kind() {
    const ret = wasm.loromap_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the size of the map.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * console.log(map.size);   // 1
   * ```
   * @returns {number}
   */
  get size() {
    const ret = wasm.loromap_size(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Delete all key-value pairs in the map.
   */
  clear() {
    const ret = wasm.loromap_clear(this.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Remove the key from the map.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * map.delete("foo");
   * ```
   * @param {string} key
   */
  delete(key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_delete(this.__wbg_ptr, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Set the key with the value.
   *
   * If the value of the key is exist, the old value will be updated.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * map.set("foo", "baz");
   * ```
   * @param {string} key
   * @param {Value} value
   */
  set(key, value) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.loromap_set(this.__wbg_ptr, ptr0, len0, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the parent container.
   *
   * - The parent container of the root tree is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.loromap_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the values of the map. If the value is a child container, the corresponding
   * `Container` will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * map.set("baz", "bar");
   * const values = map.values(); // ["bar", "bar"]
   * ```
   * @returns {any[]}
   */
  values() {
    const ret = wasm.loromap_values(this.__wbg_ptr);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the entries of the map. If the value is a child container, the corresponding
   * `Container` will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * map.set("baz", "bar");
   * const entries = map.entries(); // [["foo", "bar"], ["baz", "bar"]]
   * ```
   * @returns {([string, Value | Container])[]}
   */
  entries() {
    const ret = wasm.loromap_entries(this.__wbg_ptr);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the keys and the values. If the type of value is a child container,
   * it will be resolved recursively.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.set("foo", "bar");
   * const text = map.setContainer("text", new LoroText());
   * text.insert(0, "Hello");
   * console.log(map.toJSON());  // {"foo": "bar", "text": "Hello"}
   * ```
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.loromap_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Check if the container is deleted
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.loromap_isDeleted(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Subscribe to the changes of the map.
   *
   * Returns a subscription callback, which can be used to unsubscribe.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * @param {Listener} f - Event listener
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const map = doc.getMap("map");
   * map.subscribe((event)=>{
   *     console.log(event);
   * });
   * map.set("foo", "bar");
   * doc.commit();
   * ```
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.loromap_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.loromap_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroMovableListFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_loromovablelist_free(ptr >>> 0, 1));
var LoroMovableList = class _LoroMovableList {
  static {
    __name(this, "LoroMovableList");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroMovableList.prototype);
    obj.__wbg_ptr = ptr;
    LoroMovableListFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroMovableListFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_loromovablelist_free(ptr, 0);
  }
  /**
   * Whether the container is attached to a document.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorolist_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the creator of the list item at the given position.
   * @param {number} pos
   * @returns {PeerID | undefined}
   */
  getCreatorAt(pos) {
    const ret = wasm.loromovablelist_getCreatorAt(this.__wbg_ptr, pos);
    return ret;
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that equals to this or created by this, otherwise `undefined`.
   * @returns {LoroList | undefined}
   */
  getAttached() {
    const ret = wasm.loromovablelist_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Set the container at the given position.
   * @param {number} pos
   * @param {Container} child
   * @returns {Container}
   */
  setContainer(pos, child) {
    const ret = wasm.loromovablelist_setContainer(this.__wbg_ptr, pos, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the last mover of the list item at the given position.
   * @param {number} pos
   * @returns {PeerID | undefined}
   */
  getLastMoverAt(pos) {
    const ret = wasm.loromovablelist_getLastMoverAt(this.__wbg_ptr, pos);
    return ret;
  }
  /**
   * Push a container to the end of the list.
   * @param {Container} child
   * @returns {Container}
   */
  pushContainer(child) {
    const ret = wasm.loromovablelist_pushContainer(this.__wbg_ptr, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the last editor of the list item at the given position.
   * @param {number} pos
   * @returns {PeerID | undefined}
   */
  getLastEditorAt(pos) {
    const ret = wasm.loromovablelist_getLastEditorAt(this.__wbg_ptr, pos);
    return ret;
  }
  /**
   * Get the deep value of the elements in the range `[start, end)`.
   *
   * Each child container in the range is recursively resolved to its deep
   * value. Negative bounds are clamped to 0 and out-of-range bounds are
   * clamped to the list length; an empty or inverted range returns `[]`.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getMovableList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getRangeValue(0, 2));  // [100, "Hello"]
   * ```
   * @param {number} start
   * @param {number} end
   * @returns {any}
   */
  getRangeValue(start, end) {
    const ret = wasm.loromovablelist_getRangeValue(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Insert a container at the index.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.toJSON());  // [100, "Hello"];
   * ```
   * @param {number} index
   * @param {Container} child
   * @returns {Container}
   */
  insertContainer(index, child) {
    const ret = wasm.loromovablelist_insertContainer(this.__wbg_ptr, index, child);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the shallow value of the movable list.
   *
   * Unlike `toJSON()` which recursively resolves all containers to their values,
   * `getShallowValue()` returns container IDs as strings for any nested containers.
   *
   * ```js
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const list = doc.getMovableList("list");
   * list.insert(0, 1);
   * list.insert(1, "two");
   * const subList = list.insertContainer(2, new LoroList());
   * subList.insert(0, "sub");
   * list.getShallowValue(); // [1, "two", "cid:2@1:List"]
   * list.toJSON(); // [1, "two", ["sub"]]
   * ```
   * @returns {Value[]}
   */
  getShallowValue() {
    const ret = wasm.loromovablelist_getShallowValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the deep value of the movable list with its container id.
   *
   * The result is a `{ cid, value }` object, where `cid` is the container id
   * string (the same as `list.id`) and `value` is the deep value of the
   * list. Child containers inside `value` are recursively replaced by their
   * own `{ cid, value }` nodes. This is the same node shape that
   * `LoroDoc.getDeepValueWithID()` emits for every container.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getMovableList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getDeepValueWithID());
   * // { cid: "cid:root-list:MovableList",
   * //   value: [100, { cid: "cid:0@1:Text", value: "Hello" }] }
   * ```
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.loromovablelist_getDeepValueWithID(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the deep value of the elements in the range `[start, end)`, with
   * container ids.
   *
   * Each child container in the range is recursively replaced by its own
   * `{ cid, value }` node. Negative bounds are clamped to 0 and
   * out-of-range bounds are clamped to the list length; an empty or
   * inverted range returns `[]`.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getMovableList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.getRangeDeepValueWithID(0, 2));
   * // [100, { cid: "cid:0@1:Text", value: "Hello" }]
   * ```
   * @param {number} start
   * @param {number} end
   * @returns {any}
   */
  getRangeDeepValueWithID(start, end) {
    const ret = wasm.loromovablelist_getRangeDeepValueWithID(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the id of this container.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.loromovablelist_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the value at the index. If the value is a container, the corresponding handler will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * console.log(list.get(0));  // 100
   * console.log(list.get(1));  // undefined
   * ```
   * @param {number} index
   * @returns {Value | Container | undefined}
   */
  get(index) {
    const ret = wasm.loromovablelist_get(this.__wbg_ptr, index);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Move the element from `from` to `to`.
   *
   * The new position of the element will be `to`.
   * Move the element from `from` to `to`.
   *
   * The new position of the element will be `to`. This method is optimized to prevent redundant
   * operations that might occur with a naive remove and insert approach. Specifically, it avoids
   * creating surplus values in the list, unlike a delete followed by an insert, which can lead to
   * additional values in cases of concurrent edits. This ensures more efficient and accurate
   * operations in a MovableList.
   * @param {number} from
   * @param {number} to
   */
  move(from, to) {
    const ret = wasm.loromovablelist_move(this.__wbg_ptr, from, to);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Create a new detached LoroMovableList (not attached to any LoroDoc).
   *
   * The edits on a detached container will not be persisted.
   * To attach the container to the document, please insert it into an attached container.
   */
  constructor() {
    const ret = wasm.loromovablelist_new();
    this.__wbg_ptr = ret >>> 0;
    LoroMovableListFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Pop a value from the end of the list.
   * @returns {Value | undefined}
   */
  pop() {
    const ret = wasm.loromovablelist_pop(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Set the value at the given position.
   *
   * It's different from `delete` + `insert` that it will replace the value at the position.
   *
   * For example, if you have a list `[1, 2, 3]`, and you call `set(1, 100)`, the list will be `[1, 100, 3]`.
   * If concurrently someone call `set(1, 200)`, the list will be `[1, 200, 3]` or `[1, 100, 3]`.
   *
   * But if you use `delete` + `insert` to simulate the set operation, they may create redundant operations
   * and the final result will be `[1, 100, 200, 3]` or `[1, 200, 100, 3]`.
   * @param {number} pos
   * @param {Value} value
   */
  set(pos, value) {
    const ret = wasm.loromovablelist_set(this.__wbg_ptr, pos, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * "MovableList"
   * @returns {'MovableList'}
   */
  kind() {
    const ret = wasm.loromovablelist_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Push a value to the end of the list.
   * @param {Value} value
   */
  push(value) {
    const ret = wasm.loromovablelist_push(this.__wbg_ptr, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Delete all elements in the list.
   */
  clear() {
    const ret = wasm.loromovablelist_clear(this.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Delete elements from index to index + len.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.delete(0, 1);
   * console.log(list.value);  // []
   * ```
   * @param {number} index
   * @param {number} len
   */
  delete(index, len) {
    const ret = wasm.loromovablelist_delete(this.__wbg_ptr, index, len);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Insert a value at index.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * console.log(list.value);  // [100, "foo", true];
   * ```
   * @param {number} index
   * @param {Value} value
   */
  insert(index, value) {
    const ret = wasm.loromovablelist_insert(this.__wbg_ptr, index, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the length of list.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * console.log(list.length);  // 3
   * ```
   * @returns {number}
   */
  get length() {
    const ret = wasm.loromovablelist_length(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Get the parent container.
   *
   * - The parent container of the root tree is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.loromovablelist_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get elements of the list. If the type of a element is a container, it will be
   * resolved recursively.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * const text = list.insertContainer(1, new LoroText());
   * text.insert(0, "Hello");
   * console.log(list.toJSON());  // [100, "Hello"];
   * ```
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.loromovablelist_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get elements of the list. If the value is a child container, the corresponding
   * `Container` will be returned.
   *
   * @example
   * ```ts
   * import { LoroDoc, LoroText } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.insert(0, 100);
   * list.insert(1, "foo");
   * list.insert(2, true);
   * list.insertContainer(3, new LoroText());
   * console.log(list.value);  // [100, "foo", true, LoroText];
   * ```
   * @returns {(Value | Container)[]}
   */
  toArray() {
    const ret = wasm.loromovablelist_toArray(this.__wbg_ptr);
    if (ret[3]) {
      throw takeFromExternrefTable0(ret[2]);
    }
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the cursor of the container.
   * @param {number} pos
   * @param {Side} side
   * @returns {Cursor | undefined}
   */
  getCursor(pos, side) {
    const ret = wasm.loromovablelist_getCursor(this.__wbg_ptr, pos, side);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 0 ? void 0 : Cursor.__wrap(ret[0]);
  }
  /**
   * Check if the container is deleted
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.loromovablelist_isDeleted(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Subscribe to the changes of the list.
   *
   * Returns a subscription callback, which can be used to unsubscribe.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const list = doc.getList("list");
   * list.subscribe((event)=>{
   *     console.log(event);
   * });
   * list.insert(0, 100);
   * doc.commit();
   * ```
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.loromovablelist_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.loromovablelist_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Read [start,end), clamped to list length. Inverted bounds return no items.
   * Returns actual start, totalLength and items; this is not a complete container.
   * Only selected descendants are traversed; parent shallow list access is O(N).
   * @param {number} start
   * @param {number} end
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerTreeSlice}
   */
  toContainerTreeSlice(start, end, opts) {
    const ret = wasm.loromovablelist_toContainerTreeSlice(this.__wbg_ptr, start, end, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroTextFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorotext_free(ptr >>> 0, 1));
var LoroText = class _LoroText {
  static {
    __name(this, "LoroText");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroText.prototype);
    obj.__wbg_ptr = ptr;
    LoroTextFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroTextFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorotext_free(ptr, 0);
  }
  /**
   * Change the state of this text by delta.
   *
   * If a delta item is `insert`, it should include all the attributes of the inserted text.
   * Loro's rich text CRDT may make the inserted text inherit some styles when you use
   * `insert` method directly. However, when you use `applyDelta` if some attributes are
   * inherited from CRDT but not included in the delta, they will be removed.
   *
   * Another special property of `applyDelta` is if you format an attribute for ranges out of
   * the text length, Loro will insert new lines to fill the gap first. It's useful when you
   * build the binding between Loro and rich text editors like Quill, which might assume there
   * is always a newline at the end of the text implicitly.
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * doc.configTextStyle({bold: {expand: "after"}});
   * text.insert(0, "Hello World!");
   * text.mark({ start: 0, end: 5 }, "bold", true);
   * const delta = text.toDelta();
   * const text2 = doc.getText("text2");
   * text2.applyDelta(delta);
   * expect(text2.toDelta()).toStrictEqual(delta);
   * ```
   * @param {Delta<string>[]} delta
   */
  applyDelta(delta) {
    const ret = wasm.lorotext_applyDelta(this.__wbg_ptr, delta);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Convert a position between coordinate systems.
   *
   * Supported values: `"unicode"`, `"utf16"`, `"utf8"`.
   *
   * Returns `undefined` when out of bounds or unsupported.
   * @param {number} index
   * @param {string} from
   * @param {string} to
   * @returns {any}
   */
  convertPos(index, from, to) {
    const ptr0 = passStringToWasm0(from, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ptr1 = passStringToWasm0(to, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_convertPos(this.__wbg_ptr, index, ptr0, len0, ptr1, len1);
    return ret;
  }
  /**
   * Delete elements from index to utf-8 index + len
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insertUtf8(0, "Hello");
   * text.deleteUtf8(1, 3);
   * const s = text.toString();
   * console.log(s); // "Ho"
   * ```
   * @param {number} index
   * @param {number} len
   */
  deleteUtf8(index, len) {
    const ret = wasm.lorotext_deleteUtf8(this.__wbg_ptr, index, len);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the editor of the text at the given position.
   *
   * Returns `undefined` if the position is out of range.
   * @param {number} pos
   * @returns {PeerID | undefined}
   */
  getEditorOf(pos) {
    const ret = wasm.lorotext_getEditorOf(this.__wbg_ptr, pos);
    return ret;
  }
  /**
   * Insert some string at utf-8 index.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insertUtf8(0, "Hello");
   * ```
   * @param {number} index
   * @param {string} content
   */
  insertUtf8(index, content) {
    const ptr0 = passStringToWasm0(content, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_insertUtf8(this.__wbg_ptr, index, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Whether the container is attached to a LoroDoc.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorolist_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the rich text delta in the given range (utf-16 index).
   * @param {number} start
   * @param {number} end
   * @returns {Delta<string>[]}
   */
  sliceDelta(start, end) {
    const ret = wasm.lorotext_sliceDelta(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that is equal to this or created by this; otherwise, it returns `undefined`.
   * @returns {LoroText | undefined}
   */
  getAttached() {
    const ret = wasm.lorotext_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Update the current text to the target text, the difference is calculated line by line.
   *
   * It uses Myers' diff algorithm to compute the optimal difference.
   * @param {string} text
   * @param {any} options
   */
  updateByLine(text, options) {
    const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_updateByLine(this.__wbg_ptr, ptr0, len0, options);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the rich text delta in the given range (utf-8 index).
   * @param {number} start
   * @param {number} end
   * @returns {Delta<string>[]}
   */
  sliceDeltaUtf8(start, end) {
    const ret = wasm.lorotext_sliceDeltaUtf8(this.__wbg_ptr, start, end);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the shallow value of the text. This equals to `text.toString()`.
   * @returns {string}
   */
  getShallowValue() {
    let deferred1_0;
    let deferred1_1;
    try {
      const ret = wasm.lorotext_getShallowValue(this.__wbg_ptr);
      deferred1_0 = ret[0];
      deferred1_1 = ret[1];
      return getStringFromWasm0(ret[0], ret[1]);
    } finally {
      wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
    }
  }
  /**
   * Get the deep value of the text with its container id.
   *
   * The result is a `{ cid, value }` object, where `cid` is the container id
   * string (the same as `text.id`) and `value` is the text content. This is
   * the same node shape that `LoroDoc.getDeepValueWithID()` emits for every
   * container.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * console.log(text.getDeepValueWithID());  // { cid: "cid:root-text:Text", value: "Hello" }
   * ```
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.lorotext_getDeepValueWithID(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the container id of the text.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.lorotext_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Create a new detached LoroText (not attached to any LoroDoc).
   *
   * The edits on a detached container will not be persisted.
   * To attach the container to the document, please insert it into an attached container.
   */
  constructor() {
    const ret = wasm.lorotext_new();
    this.__wbg_ptr = ret >>> 0;
    LoroTextFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Iterate each text span(internal storage unit)
   *
   * The callback function will be called for each span in the text.
   * If the callback returns `false`, the iteration will stop.
   *
   * The current text chunks are snapshotted before callbacks run, so the callback
   * may read or mutate the document without re-entering internal state locks.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.iter((str) => (console.log(str), true));
   * ```
   * @param {(string) => boolean} callback
   */
  iter(callback) {
    const ret = wasm.lorotext_iter(this.__wbg_ptr, callback);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * "Text"
   * @returns {'Text'}
   */
  kind() {
    const ret = wasm.lorotext_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Mark a range of text with a key and a value (utf-16 index).
   *
   * > You should call `configTextStyle` before using `mark` and `unmark`.
   *
   * You can use it to create a highlight, make a range of text bold, or add a link to a range of text.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.configTextStyle({bold: {expand: "after"}});
   * const text = doc.getText("text");
   * text.insert(0, "Hello World!");
   * text.mark({ start: 0, end: 5 }, "bold", true);
   * ```
   * @param {{ start: number, end: number }} range
   * @param {string} key
   * @param {any} value
   */
  mark(range, key, value) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_mark(this.__wbg_ptr, range, ptr0, len0, value);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Push a string to the end of the text.
   * @param {string} s
   */
  push(s) {
    const ptr0 = passStringToWasm0(s, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_push(this.__wbg_ptr, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get a string slice (utf-16 index).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.slice(0, 2); // "He"
   * ```
   * @param {number} start_index
   * @param {number} end_index
   * @returns {string}
   */
  slice(start_index, end_index) {
    let deferred2_0;
    let deferred2_1;
    try {
      const ret = wasm.lorotext_slice(this.__wbg_ptr, start_index, end_index);
      var ptr1 = ret[0];
      var len1 = ret[1];
      if (ret[3]) {
        ptr1 = 0;
        len1 = 0;
        throw takeFromExternrefTable0(ret[2]);
      }
      deferred2_0 = ptr1;
      deferred2_1 = len1;
      return getStringFromWasm0(ptr1, len1);
    } finally {
      wasm.__wbindgen_free(deferred2_0, deferred2_1, 1);
    }
  }
  /**
   * Delete elements from index to index + len (utf-16 index).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.delete(1, 3);
   * const s = text.toString();
   * console.log(s); // "Ho"
   * ```
   * @param {number} index
   * @param {number} len
   */
  delete(index, len) {
    const ret = wasm.lorotext_delete(this.__wbg_ptr, index, len);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Insert the string at the given index (utf-16 index).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * ```
   * @param {number} index
   * @param {string} content
   */
  insert(index, content) {
    const ptr0 = passStringToWasm0(content, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_insert(this.__wbg_ptr, index, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the length of text (utf-16 length).
   * @returns {number}
   */
  get length() {
    const ret = wasm.lorotext_length(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * Get the parent container.
   *
   * - The parent of the root is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.lorotext_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Delete and return the string at the given range and insert a string at the same position (utf-16 index).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.splice(2, 3, "llo"); // "llo"
   * ```
   * @param {number} pos
   * @param {number} len
   * @param {string} s
   * @returns {string}
   */
  splice(pos, len, s) {
    let deferred3_0;
    let deferred3_1;
    try {
      const ptr0 = passStringToWasm0(s, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
      const len0 = WASM_VECTOR_LEN;
      const ret = wasm.lorotext_splice(this.__wbg_ptr, pos, len, ptr0, len0);
      var ptr2 = ret[0];
      var len2 = ret[1];
      if (ret[3]) {
        ptr2 = 0;
        len2 = 0;
        throw takeFromExternrefTable0(ret[2]);
      }
      deferred3_0 = ptr2;
      deferred3_1 = len2;
      return getStringFromWasm0(ptr2, len2);
    } finally {
      wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
    }
  }
  /**
   * Unmark a range of text with a key and a value (utf-16 index).
   *
   * > You should call `configTextStyle` before using `mark` and `unmark`.
   *
   * You can use it to remove highlights, bolds or links
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * doc.configTextStyle({bold: {expand: "after"}});
   * const text = doc.getText("text");
   * text.insert(0, "Hello World!");
   * text.mark({ start: 0, end: 5 }, "bold", true);
   * text.unmark({ start: 0, end: 5 }, "bold");
   * ```
   * @param {{ start: number, end: number }} range
   * @param {string} key
   */
  unmark(range, key) {
    const ptr0 = passStringToWasm0(key, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_unmark(this.__wbg_ptr, range, ptr0, len0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Update the current text to the target text.
   *
   * It will calculate the minimal difference and apply it to the current text.
   * It uses Myers' diff algorithm to compute the optimal difference.
   *
   * This could take a long time for large texts (e.g. > 50_000 characters).
   * In that case, you should use `updateByLine` instead.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.update("Hello World");
   * console.log(text.toString()); // "Hello World"
   * ```
   * @param {string} text
   * @param {any} options
   */
  update(text, options) {
    const ptr0 = passStringToWasm0(text, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.lorotext_update(this.__wbg_ptr, ptr0, len0, options);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the character at the given position (utf-16 index).
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * text.insert(0, "Hello");
   * text.charAt(0); // "H"
   * ```
   * @param {number} pos
   * @returns {string}
   */
  charAt(pos) {
    const ret = wasm.lorotext_charAt(this.__wbg_ptr, pos);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return String.fromCodePoint(ret[0]);
  }
  /**
   * Get the JSON representation of the text.
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.lorotext_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the text in [Delta](https://quilljs.com/docs/delta/) format.
   *
   * The returned value will include the rich text information.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const text = doc.getText("text");
   * doc.configTextStyle({bold: {expand: "after"}});
   * text.insert(0, "Hello World!");
   * text.mark({ start: 0, end: 5 }, "bold", true);
   * console.log(text.toDelta());  // [ { insert: 'Hello', attributes: { bold: true } } ]
   * ```
   * @returns {Delta<string>[]}
   */
  toDelta() {
    const ret = wasm.lorotext_toDelta(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the cursor at the given position.
   *
   * - The first argument is the position (utf16-index).
   * - The second argument is the side: `-1` for left, `0` for middle, `1` for right.
   * @param {number} pos
   * @param {Side} side
   * @returns {Cursor | undefined}
   */
  getCursor(pos, side) {
    const ret = wasm.lorotext_getCursor(this.__wbg_ptr, pos, side);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 0 ? void 0 : Cursor.__wrap(ret[0]);
  }
  /**
   * Check if the container is deleted
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.lorotext_isDeleted(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Subscribe to the changes of the text.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * returns a subscription callback, which can be used to unsubscribe.
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.lorotext_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert the text to a string
   * @returns {string}
   */
  toString() {
    let deferred1_0;
    let deferred1_1;
    try {
      const ret = wasm.lorotext_toString(this.__wbg_ptr);
      deferred1_0 = ret[0];
      deferred1_1 = ret[1];
      return getStringFromWasm0(ret[0], ret[1]);
    } finally {
      wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
    }
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.lorotext_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroTreeFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorotree_free(ptr >>> 0, 1));
var LoroTree = class _LoroTree {
  static {
    __name(this, "LoroTree");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroTree.prototype);
    obj.__wbg_ptr = ptr;
    LoroTreeFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroTreeFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorotree_free(ptr, 0);
  }
  /**
   * Create a new tree node as the child of parent and return a `LoroTreeNode` instance.
   * If the parent is undefined, the tree node will be a root node.
   *
   * If the index is not provided, the new node will be appended to the end.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = tree.createNode(undefined, 0);
   *
   * //  undefined
   * //    /   \
   * // node  root
   * ```
   * @param {TreeID | undefined} parent
   * @param {number | null} [index]
   * @returns {LoroTreeNode}
   */
  createNode(parent, index) {
    const ret = wasm.lorotree_createNode(this.__wbg_ptr, parent, isLikeNone(index) ? 4294967297 : index >>> 0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroTreeNode.__wrap(ret[0]);
  }
  /**
   * Whether the container is attached to a document.
   *
   * If it's detached, the operations on the container will not be persisted.
   * @returns {boolean}
   */
  isAttached() {
    const ret = wasm.lorolist_isAttached(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the attached container associated with this.
   *
   * Returns an attached `Container` that equals to this or created by this, otherwise `undefined`.
   * @returns {LoroTree | undefined}
   */
  getAttached() {
    const ret = wasm.lorotree_getAttached(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get LoroTreeNode by the TreeID.
   * @param {TreeID} target
   * @returns {LoroTreeNode | undefined}
   */
  getNodeByID(target) {
    const ret = wasm.lorotree_getNodeByID(this.__wbg_ptr, target);
    return ret === 0 ? void 0 : LoroTreeNode.__wrap(ret);
  }
  /**
   * Return `None` if the node is not exist, otherwise return `Some(true)` if the node is deleted.
   * @param {TreeID} target
   * @returns {boolean}
   */
  isNodeDeleted(target) {
    const ret = wasm.lorotree_isNodeDeleted(this.__wbg_ptr, target);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
  }
  /**
   * Get the shallow value of the tree.
   *
   * Unlike `toJSON()` which recursively resolves nested containers to their values,
   * `getShallowValue()` returns container IDs as strings for any nested containers.
   *
   * @example
   * ```ts
   * const doc = new LoroDoc();
   * doc.setPeerId("1");
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * root.data.set("name", "root");
   * const text = root.data.setContainer("content", new LoroText());
   * text.insert(0, "Hello");
   *
   * console.log(tree.getShallowValue());
   * // [{
   * //   id: "0@1",
   * //   parent: null,
   * //   index: 0,
   * //   fractional_index: "80",
   * //   meta: "cid:0@1:Map",
   * //   children: []
   * // }]
   *
   * console.log(tree.toJSON());
   * // [{
   * //   id: "0@1",
   * //   parent: null,
   * //   index: 0,
   * //   fractional_index: "80",
   * //   meta: {
   * //     name: "root",
   * //     content: "Hello"
   * //   },
   * //   children: []
   * // }]
   * ```
   * @returns {TreeNodeShallowValue[]}
   */
  getShallowValue() {
    const ret = wasm.lorotree_getShallowValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the deep value of the tree with its container id.
   *
   * The result is a `{ cid, value }` object, where `cid` is the container id
   * string (the same as `tree.id`) and `value` is the deep value of the
   * tree (each node's `meta` is the deep value of its associated metadata
   * map). This is the same node shape that `LoroDoc.getDeepValueWithID()`
   * emits for every container.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * root.data.set("color", "red");
   * console.log(tree.getDeepValueWithID());
   * // { cid: "cid:root-tree:Tree",
   * //   value: [ { id: '0@1', parent: null, ..., meta: { color: 'red' }, children: [] } ] }
   * ```
   * @returns {any}
   */
  getDeepValueWithID() {
    const ret = wasm.lorotree_getDeepValueWithID(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Set whether to generate a fractional index for moving and creating.
   *
   * A fractional index can be used to determine the position of tree nodes among their siblings.
   *
   * The jitter is used to avoid conflicts when multiple users are creating a node at the same position.
   * A value of 0 is the default, which means no jitter; any value larger than 0 will enable jitter.
   *
   * Generally speaking, higher jitter value will increase the size of the operation
   * [Read more about it](https://www.loro.dev/blog/movable-tree#implementation-and-encoding-size)
   * @param {number} jitter
   */
  enableFractionalIndex(jitter) {
    wasm.lorotree_enableFractionalIndex(this.__wbg_ptr, jitter);
  }
  /**
   * Disable the fractional index generation when you don't need the Tree's siblings to be sorted.
   * The fractional index will always be set to the same default value 0.
   *
   * After calling this, you cannot use `tree.moveTo()`, `tree.moveBefore()`, `tree.moveAfter()`,
   * and `tree.createAt()`.
   */
  disableFractionalIndex() {
    wasm.lorotree_disableFractionalIndex(this.__wbg_ptr);
  }
  /**
   * Whether the tree enables the fractional index generation.
   * @returns {boolean}
   */
  isFractionalIndexEnabled() {
    const ret = wasm.lorotree_isFractionalIndexEnabled(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Get the id of the container.
   * @returns {ContainerID}
   */
  get id() {
    const ret = wasm.lorotree_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * Move the target tree node to be a child of the parent.
   * It's not allowed that the target is an ancestor of the parent
   * or the target and the parent are the same node.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = root.createNode();
   * const node2 = node.createNode();
   * tree.move(node2.id, root.id);
   * // Error will be thrown if move operation creates a cycle
   * // tree.move(root.id, node.id);
   * ```
   * @param {TreeID} target
   * @param {TreeID | undefined} parent
   * @param {number | null} [index]
   */
  move(target, parent, index) {
    const ret = wasm.lorotree_move(this.__wbg_ptr, target, parent, isLikeNone(index) ? 4294967297 : index >>> 0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Create a new detached LoroTree (not attached to any LoroDoc).
   *
   * The edits on a detached container will not be persisted.
   * To attach the container to the document, please insert it into an attached container.
   */
  constructor() {
    const ret = wasm.lorotree_new();
    this.__wbg_ptr = ret >>> 0;
    LoroTreeFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * "Tree"
   * @returns {'Tree'}
   */
  kind() {
    const ret = wasm.lorotree_kind(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get all tree nodes of the forest, including deleted nodes.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = root.createNode();
   * const node2 = node.createNode();
   * console.log(tree.nodes());
   * ```
   * @returns {LoroTreeNode[]}
   */
  nodes() {
    const ret = wasm.lorotree_nodes(this.__wbg_ptr);
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Get the root nodes of the forest.
   * @returns {LoroTreeNode[]}
   */
  roots() {
    const ret = wasm.lorotree_roots(this.__wbg_ptr);
    var v1 = getArrayJsValueFromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 4, 4);
    return v1;
  }
  /**
   * Delete a tree node from the forest.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = root.createNode();
   * tree.delete(node.id);
   * ```
   * @param {TreeID} target
   */
  delete(target) {
    const ret = wasm.lorotree_delete(this.__wbg_ptr, target);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the parent container of the tree container.
   *
   * - The parent container of the root tree is `undefined`.
   * - The object returned is a new js object each time because it need to cross
   *   the WASM boundary.
   * @returns {Container | undefined}
   */
  parent() {
    const ret = wasm.lorotree_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the hierarchy array with metadata of the forest.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * root.data.set("color", "red");
   * // [ { id: '0@F2462C4159C4C8D1', parent: null, meta: { color: 'red' }, children: [] } ]
   * console.log(tree.toJSON());
   * ```
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.lorotree_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Return `true` if the tree contains the TreeID, include deleted node.
   * @param {TreeID} target
   * @returns {boolean}
   */
  has(target) {
    const ret = wasm.lorotree_has(this.__wbg_ptr, target);
    return ret !== 0;
  }
  /**
   * Get the hierarchy array of the forest.
   *
   * Note: the metadata will be not resolved. So if you don't only care about hierarchy
   * but also the metadata, you should use `toJson()`.
   * @returns {Array<any>}
   */
  toArray() {
    const ret = wasm.lorotree_toArray(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the flat array of the forest. If `with_deleted` is true, the deleted nodes will be included.
   * @param {{ withDeleted: boolean }} options
   * @returns {Array<any>}
   */
  getNodes(options) {
    const ret = wasm.lorotree_getNodes(this.__wbg_ptr, options);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Check if the container is deleted
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.lorotree_isDeleted(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Subscribe to the changes of the tree.
   *
   * Returns a subscription callback, which can be used to unsubscribe.
   *
   * Trees have three types of events: `create`, `delete`, and `move`.
   * - `create`: Creates a new node with its `target` TreeID. If `parent` is undefined,
   *             a root node is created; otherwise, a child node of `parent` is created.
   *             If the node being created was previously deleted and has archived child nodes,
   *             create events for these child nodes will also be received.
   * - `delete`: Deletes the target node. The structure and state of the target node and
   *             its child nodes are archived, and delete events for the child nodes will not be received.
   * - `move`:   Moves the target node. If `parent` is undefined, the target node becomes a root node;
   *             otherwise, it becomes a child node of `parent`.
   *
   * If a tree container is subscribed, the event of metadata changes will also be received as a MapDiff.
   * And event's `path` will end with `TreeID`.
   *
   * The events will be emitted after a transaction is committed. A transaction is committed when:
   *
   * - `doc.commit()` is called.
   * - `doc.export(mode)` is called.
   * - `doc.import(data)` is called.
   * - `doc.checkout(version)` is called.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * tree.subscribe((event)=>{
   *     // event.type: "create" | "delete" | "move"
   * });
   * const root = tree.createNode();
   * const node = root.createNode();
   * doc.commit();
   * ```
   * @param {Function} f
   * @returns {any}
   */
  subscribe(f) {
    const ret = wasm.lorotree_subscribe(this.__wbg_ptr, f);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Convert this attached container and all descendants to an independent tree.
   * Text format applies recursively. Does not commit; throws for detached
   * containers, unsupported types, invalid options, or nesting above 256.
   * @param {ContainerTreeOptions | null} [opts]
   * @returns {ContainerNode}
   */
  toContainerTree(opts) {
    const ret = wasm.lorotree_toContainerTree(this.__wbg_ptr, isLikeNone(opts) ? 0 : addToExternrefTable0(opts));
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
};
var LoroTreeNodeFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_lorotreenode_free(ptr >>> 0, 1));
var LoroTreeNode = class _LoroTreeNode {
  static {
    __name(this, "LoroTreeNode");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_LoroTreeNode.prototype);
    obj.__wbg_ptr = ptr;
    LoroTreeNodeFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    LoroTreeNodeFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_lorotreenode_free(ptr, 0);
  }
  /**
   * @returns {string}
   */
  __getClassname() {
    let deferred1_0;
    let deferred1_1;
    try {
      const ret = wasm.lorotreenode___getClassname(this.__wbg_ptr);
      deferred1_0 = ret[0];
      deferred1_1 = ret[1];
      return getStringFromWasm0(ret[0], ret[1]);
    } finally {
      wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
    }
  }
  /**
   * Get the creation id of this node.
   * @returns {{ peer: PeerID, counter: number }}
   */
  creationId() {
    const ret = wasm.lorotreenode_creationId(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Check if the node is deleted.
   * @returns {boolean}
   */
  isDeleted() {
    const ret = wasm.lorotreenode_isDeleted(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
  }
  /**
   * Move the tree node to be before the target node.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = root.createNode();
   * const node2 = root.createNode();
   * node2.moveBefore(node);
   * //   root
   * //  /    \
   * // node2 node
   * ```
   * @param {LoroTreeNode} target
   */
  moveBefore(target) {
    _assertClass(target, _LoroTreeNode);
    const ret = wasm.lorotreenode_moveBefore(this.__wbg_ptr, target.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Create a new node as the child of the current node and
   * return an instance of `LoroTreeNode`.
   *
   * If the index is not provided, the new node will be appended to the end.
   *
   * @example
   * ```typescript
   * import { LoroDoc } from "loro-crdt";
   *
   * let doc = new LoroDoc();
   * let tree = doc.getTree("tree");
   * let root = tree.createNode();
   * let node = root.createNode();
   * let node2 = root.createNode(0);
   * //    root
   * //    /  \
   * // node2 node
   * ```
   * @param {number | null} [index]
   * @returns {LoroTreeNode}
   */
  createNode(index) {
    const ret = wasm.lorotreenode_createNode(this.__wbg_ptr, isLikeNone(index) ? 4294967297 : index >>> 0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _LoroTreeNode.__wrap(ret[0]);
  }
  /**
   * Get the last mover of this node.
   * @returns {{ peer: PeerID, counter: number } | undefined}
   */
  getLastMoveId() {
    const ret = wasm.lorotreenode_getLastMoveId(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * @returns {any}
   */
  toJSON() {
    const ret = wasm.lorotreenode_toJSON(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * Get the `Fractional Index` of the node.
   *
   * Note: the tree container must be attached to the document.
   * @returns {string | undefined}
   */
  fractionalIndex() {
    const ret = wasm.lorotreenode_fractionalIndex(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return takeFromExternrefTable0(ret[0]);
  }
  /**
   * The TreeID of the node.
   * @returns {TreeID}
   */
  get id() {
    const ret = wasm.lorotreenode_id(this.__wbg_ptr);
    return ret;
  }
  /**
   * @param {LoroTreeNode | undefined} parent
   * @param {number | null} [index]
   */
  move(parent, index) {
    const ret = wasm.lorotreenode_move(this.__wbg_ptr, parent, isLikeNone(index) ? 4294967297 : index >>> 0);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the associated metadata map container of a tree node.
   * @returns {LoroMap}
   */
  get data() {
    const ret = wasm.lorotreenode_data(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return LoroMap.__wrap(ret[0]);
  }
  /**
   * Get the index of the node in the parent's children.
   * @returns {number | undefined}
   */
  index() {
    const ret = wasm.lorotreenode_index(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 4294967297 ? void 0 : ret[0];
  }
  /**
   * @returns {LoroTreeNode | undefined}
   */
  parent() {
    const ret = wasm.lorotreenode_parent(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 0 ? void 0 : _LoroTreeNode.__wrap(ret[0]);
  }
  /**
   * Get the creator of this node.
   * @returns {PeerID}
   */
  creator() {
    const ret = wasm.lorotreenode_creator(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the children of this node.
   *
   * The objects returned are new js objects each time because they need to cross
   * the WASM boundary.
   * @returns {any}
   */
  children() {
    const ret = wasm.lorotreenode_children(this.__wbg_ptr);
    return ret;
  }
  /**
   * Move the tree node to be after the target node.
   *
   * @example
   * ```ts
   * import { LoroDoc } from "loro-crdt";
   *
   * const doc = new LoroDoc();
   * const tree = doc.getTree("tree");
   * const root = tree.createNode();
   * const node = root.createNode();
   * const node2 = root.createNode();
   * node2.moveAfter(node);
   * // root
   * //  /  \
   * // node node2
   * ```
   * @param {LoroTreeNode} target
   */
  moveAfter(target) {
    _assertClass(target, _LoroTreeNode);
    const ret = wasm.lorotreenode_moveAfter(this.__wbg_ptr, target.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
};
var UndoManagerFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_undomanager_free(ptr >>> 0, 1));
var UndoManager = class {
  static {
    __name(this, "UndoManager");
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    UndoManagerFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_undomanager_free(ptr, 0);
  }
  groupStart() {
    const ret = wasm.undomanager_groupStart(this.__wbg_ptr);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Get the value associated with the top redo stack item, if any.
   * Returns `undefined` if there is no redo item.
   * @returns {Value | undefined}
   */
  topRedoValue() {
    const ret = wasm.undomanager_topRedoValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * Get the value associated with the top undo stack item, if any.
   * Returns `undefined` if there is no undo item.
   * @returns {Value | undefined}
   */
  topUndoValue() {
    const ret = wasm.undomanager_topUndoValue(this.__wbg_ptr);
    return ret;
  }
  /**
   * The number of max undo steps.
   * If the number of undo steps exceeds this number, the oldest undo step will be removed.
   * @param {number} steps
   */
  setMaxUndoSteps(steps) {
    wasm.undomanager_setMaxUndoSteps(this.__wbg_ptr, steps);
  }
  /**
   * Set the merge interval (in ms).
   *
   * If the interval is set to 0, the undo steps will not be merged.
   * Otherwise, the undo steps will be merged if the interval between the two steps is less than the given interval.
   * @param {number} interval
   */
  setMergeInterval(interval) {
    wasm.undomanager_setMergeInterval(this.__wbg_ptr, interval);
  }
  /**
   * If a local event's origin matches the given prefix, it will not be recorded in the
   * undo stack.
   * @param {string} prefix
   */
  addExcludeOriginPrefix(prefix) {
    const ptr0 = passStringToWasm0(prefix, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    wasm.undomanager_addExcludeOriginPrefix(this.__wbg_ptr, ptr0, len0);
  }
  /**
   * `UndoManager` is responsible for handling undo and redo operations.
   *
   * PeerID cannot be changed during the lifetime of the UndoManager.
   *
   * Note that undo operations are local and cannot revert changes made by other peers.
   * To undo changes made by other peers, consider using the time travel feature.
   *
   * Each commit made by the current peer is recorded as an undo step in the `UndoManager`.
   * Undo steps can be merged if they occur within a specified merge interval.
   *
   * ## Config
   *
   * - `mergeInterval`: Optional. The interval in milliseconds within which undo steps can be merged. Default is 1000 ms.
   * - `maxUndoSteps`: Optional. The maximum number of undo steps to retain. Default is 100.
   * - `excludeOriginPrefixes`: Optional. An array of string prefixes. Events with origins matching these prefixes will be excluded from undo steps.
   * - `onPush`: Optional. A callback function that is called when an undo/redo step is pushed.
   *    The function can return a meta data value that will be attached to the given stack item.
   * - `onPop`: Optional. A callback function that is called when an undo/redo step is popped.
   *    The function will have a meta data value that was attached to the given stack item when
   *   `onPush` was called.
   * @param {LoroDoc} doc
   * @param {UndoConfig} config
   */
  constructor(doc, config) {
    _assertClass(doc, LoroDoc);
    const ret = wasm.undomanager_new(doc.__wbg_ptr, config);
    this.__wbg_ptr = ret >>> 0;
    UndoManagerFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Get the peer id of the undo manager.
   * @returns {PeerID}
   */
  peer() {
    const ret = wasm.undomanager_peer(this.__wbg_ptr);
    return ret;
  }
  /**
   * Redo the last undone operation.
   *
   * Throws like `undo` on steps that would recreate a container of an
   * unknown type.
   * @returns {boolean}
   */
  redo() {
    const ret = wasm.undomanager_redo(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
  }
  /**
   * Undo the last operation.
   *
   * Throws without changing the doc if the step would have to recreate a
   * container of a type unknown to this version (created by a newer
   * loro-crdt). That step, with every edit merged into it, is dropped and its
   * edits stay; the next call undoes the step before it, rebased over the
   * dropped step's edits like over a remote peer's.
   * @returns {boolean}
   */
  undo() {
    const ret = wasm.undomanager_undo(this.__wbg_ptr);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] !== 0;
  }
  clear() {
    wasm.undomanager_clear(this.__wbg_ptr);
  }
  /**
   * Pause the UndoManager so that local edits and checkout events are ignored.
   *
   * While paused, local edits are not recorded as undo steps and checkout
   * events do not clear the stacks. Import events (remote changes) are still
   * processed so that the stacks remain correctly transformed against
   * concurrent edits.
   *
   * Use this before temporary checkouts (e.g. read-only history preview) that
   * should not disturb undo/redo history. Close any open group before pausing.
   *
   * Call `resume()` after returning the document to its original state.
   */
  pause() {
    wasm.undomanager_pause(this.__wbg_ptr);
  }
  /**
   * Resume the UndoManager after a `pause()`.
   */
  resume() {
    wasm.undomanager_resume(this.__wbg_ptr);
  }
  /**
   * Can redo the last operation.
   * @returns {boolean}
   */
  canRedo() {
    const ret = wasm.undomanager_canRedo(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Can undo the last operation.
   * @returns {boolean}
   */
  canUndo() {
    const ret = wasm.undomanager_canUndo(this.__wbg_ptr);
    return ret !== 0;
  }
  groupEnd() {
    wasm.undomanager_groupEnd(this.__wbg_ptr);
  }
  /**
   * Returns whether the UndoManager is currently paused.
   * @returns {boolean}
   */
  isPaused() {
    const ret = wasm.undomanager_isPaused(this.__wbg_ptr);
    return ret !== 0;
  }
  /**
   * Set the on pop event listener.
   *
   * Every time an undo step or redo step is popped, the on pop event listener will be called.
   * @param {any} on_pop
   */
  setOnPop(on_pop) {
    wasm.undomanager_setOnPop(this.__wbg_ptr, on_pop);
  }
  /**
   * Clear only the redo stack, preserving the undo stack.
   *
   * This is useful when coordinating undo/redo across multiple participants
   * (e.g., multiple editors) where a new edit in one participant should
   * invalidate redo in all other participants.
   */
  clearRedo() {
    wasm.undomanager_clearRedo(this.__wbg_ptr);
  }
  /**
   * Clear only the undo stack, preserving the redo stack.
   */
  clearUndo() {
    wasm.undomanager_clearUndo(this.__wbg_ptr);
  }
  /**
   * Set the on push event listener.
   *
   * Every time an undo step or redo step is pushed, the on push event listener will be called.
   * @param {any} on_push
   */
  setOnPush(on_push) {
    wasm.undomanager_setOnPush(this.__wbg_ptr, on_push);
  }
};
var VersionVectorFinalization = typeof FinalizationRegistry === "undefined" ? { register: /* @__PURE__ */ __name(() => {
}, "register"), unregister: /* @__PURE__ */ __name(() => {
}, "unregister") } : new FinalizationRegistry((ptr) => wasm.__wbg_versionvector_free(ptr >>> 0, 1));
var VersionVector = class _VersionVector {
  static {
    __name(this, "VersionVector");
  }
  static __wrap(ptr) {
    ptr = ptr >>> 0;
    const obj = Object.create(_VersionVector.prototype);
    obj.__wbg_ptr = ptr;
    VersionVectorFinalization.register(obj, obj.__wbg_ptr, obj);
    return obj;
  }
  __destroy_into_raw() {
    const ptr = this.__wbg_ptr;
    this.__wbg_ptr = 0;
    VersionVectorFinalization.unregister(this);
    return ptr;
  }
  free() {
    const ptr = this.__destroy_into_raw();
    wasm.__wbg_versionvector_free(ptr, 0);
  }
  /**
   * Get the counter of a peer.
   * @param {number | bigint | `${number}`} peer_id
   * @returns {number | undefined}
   */
  get(peer_id) {
    const ret = wasm.versionvector_get(this.__wbg_ptr, peer_id);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return ret[0] === 4294967297 ? void 0 : ret[0];
  }
  /**
   * Create a new version vector.
   * @param {Map<PeerID, number> | Uint8Array | VersionVector | undefined | null} value
   */
  constructor(value) {
    const ret = wasm.versionvector_new(value);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    this.__wbg_ptr = ret[0] >>> 0;
    VersionVectorFinalization.register(this, this.__wbg_ptr, this);
    return this;
  }
  /**
   * Decode the version vector from a Uint8Array.
   * @param {Uint8Array} bytes
   * @returns {VersionVector}
   */
  static decode(bytes) {
    const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.versionvector_decode(ptr0, len0);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _VersionVector.__wrap(ret[0]);
  }
  /**
   * Encode the version vector into a Uint8Array.
   * @returns {Uint8Array}
   */
  encode() {
    const ret = wasm.versionvector_encode(this.__wbg_ptr);
    var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v1;
  }
  /**
   * @returns {number}
   */
  length() {
    const ret = wasm.versionvector_length(this.__wbg_ptr);
    return ret >>> 0;
  }
  /**
   * @param {PeerID} peer
   */
  remove(peer) {
    const ret = wasm.versionvector_remove(this.__wbg_ptr, peer);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * set the exclusive ending point. target id will NOT be included by self
   * @param {{ peer: PeerID, counter: number }} id
   */
  setEnd(id) {
    const ret = wasm.versionvector_setEnd(this.__wbg_ptr, id);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Compare the version vector with another version vector.
   *
   * If they are concurrent, return undefined.
   * @param {VersionVector} other
   * @returns {number | undefined}
   */
  compare(other) {
    _assertClass(other, _VersionVector);
    const ret = wasm.versionvector_compare(this.__wbg_ptr, other.__wbg_ptr);
    return ret === 4294967297 ? void 0 : ret;
  }
  /**
   * set the inclusive ending point. target id will be included
   * @param {{ peer: PeerID, counter: number }} id
   */
  setLast(id) {
    const ret = wasm.versionvector_setLast(this.__wbg_ptr, id);
    if (ret[1]) {
      throw takeFromExternrefTable0(ret[0]);
    }
  }
  /**
   * Convert the version vector to a Map
   * @returns {Map<PeerID, number>}
   */
  toJSON() {
    const ret = wasm.versionvector_toJSON(this.__wbg_ptr);
    return ret;
  }
  /**
   * Create a new version vector from a Map.
   * @param {Map<PeerID, number>} version
   * @returns {VersionVector}
   */
  static parseJSON(version) {
    const ret = wasm.versionvector_parseJSON(version);
    if (ret[2]) {
      throw takeFromExternrefTable0(ret[1]);
    }
    return _VersionVector.__wrap(ret[0]);
  }
};
async function __wbg_load(module3, imports) {
  if (typeof Response === "function" && module3 instanceof Response) {
    if (typeof WebAssembly.instantiateStreaming === "function") {
      try {
        return await WebAssembly.instantiateStreaming(module3, imports);
      } catch (e) {
        if (module3.headers.get("Content-Type") != "application/wasm") {
          console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);
        } else {
          throw e;
        }
      }
    }
    const bytes = await module3.arrayBuffer();
    return await WebAssembly.instantiate(bytes, imports);
  } else {
    const instance = await WebAssembly.instantiate(module3, imports);
    if (instance instanceof WebAssembly.Instance) {
      return { instance, module: module3 };
    } else {
      return instance;
    }
  }
}
__name(__wbg_load, "__wbg_load");
function __wbg_get_imports() {
  const imports = {};
  imports.wbg = {};
  imports.wbg.__wbg_String_8f0eb39a4a4c2f66 = function(arg0, arg1) {
    const ret = String(arg1);
    const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
    getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
  };
  imports.wbg.__wbg_apply_36be6a55257c99bf = function() {
    return handleError(function(arg0, arg1, arg2) {
      const ret = arg0.apply(arg1, arg2);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_apply_eb9e9b97497f91e4 = function() {
    return handleError(function(arg0, arg1, arg2) {
      const ret = Reflect.apply(arg0, arg1, arg2);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_buffer_609cc3eee51ed158 = function(arg0) {
    const ret = arg0.buffer;
    return ret;
  };
  imports.wbg.__wbg_call_672a4d21634d4a24 = function() {
    return handleError(function(arg0, arg1) {
      const ret = arg0.call(arg1);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_call_7cccdd69e0791ae2 = function() {
    return handleError(function(arg0, arg1, arg2) {
      const ret = arg0.call(arg1, arg2);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_call_833bed5770ea2041 = function() {
    return handleError(function(arg0, arg1, arg2, arg3) {
      const ret = arg0.call(arg1, arg2, arg3);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_call_b8adc8b1d0a0d8eb = function() {
    return handleError(function(arg0, arg1, arg2, arg3, arg4) {
      const ret = arg0.call(arg1, arg2, arg3, arg4);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_changemodifier_new = function(arg0) {
    const ret = ChangeModifier.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_crypto_574e78ad8b13b65f = function(arg0) {
    const ret = arg0.crypto;
    return ret;
  };
  imports.wbg.__wbg_cursor_new = function(arg0) {
    const ret = Cursor.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_done_769e5ede4b31c67b = function(arg0) {
    const ret = arg0.done;
    return ret;
  };
  imports.wbg.__wbg_entries_3265d4158b33e5dc = function(arg0) {
    const ret = Object.entries(arg0);
    return ret;
  };
  imports.wbg.__wbg_entries_c8a90a7ed73e84ce = function(arg0) {
    const ret = arg0.entries();
    return ret;
  };
  imports.wbg.__wbg_error_1d6f2b3f2c5a33ec = function(arg0, arg1) {
    console.error(getStringFromWasm0(arg0, arg1));
  };
  imports.wbg.__wbg_from_2a5d3e218e67aa85 = function(arg0) {
    const ret = Array.from(arg0);
    return ret;
  };
  imports.wbg.__wbg_getOwnPropertySymbols_97eebed6fe6e08be = function(arg0) {
    const ret = Object.getOwnPropertySymbols(arg0);
    return ret;
  };
  imports.wbg.__wbg_getRandomValues_b8f5dbd5f3995a9e = function() {
    return handleError(function(arg0, arg1) {
      arg0.getRandomValues(arg1);
    }, arguments);
  };
  imports.wbg.__wbg_get_67b2ba62fc30de12 = function() {
    return handleError(function(arg0, arg1) {
      const ret = Reflect.get(arg0, arg1);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_get_b9b93047fe3cf45b = function(arg0, arg1) {
    const ret = arg0[arg1 >>> 0];
    return ret;
  };
  imports.wbg.__wbg_getindex_5b00c274b05714aa = function(arg0, arg1) {
    const ret = arg0[arg1 >>> 0];
    return ret;
  };
  imports.wbg.__wbg_getwithrefkey_1dc361bd10053bfe = function(arg0, arg1) {
    const ret = arg0[arg1];
    return ret;
  };
  imports.wbg.__wbg_instanceof_ArrayBuffer_e14585432e3737fc = function(arg0) {
    let result;
    try {
      result = arg0 instanceof ArrayBuffer;
    } catch (_) {
      result = false;
    }
    const ret = result;
    return ret;
  };
  imports.wbg.__wbg_instanceof_Map_f3469ce2244d2430 = function(arg0) {
    let result;
    try {
      result = arg0 instanceof Map;
    } catch (_) {
      result = false;
    }
    const ret = result;
    return ret;
  };
  imports.wbg.__wbg_instanceof_Object_7f2dcef8f78644a4 = function(arg0) {
    let result;
    try {
      result = arg0 instanceof Object;
    } catch (_) {
      result = false;
    }
    const ret = result;
    return ret;
  };
  imports.wbg.__wbg_instanceof_Uint8Array_17156bcf118086a9 = function(arg0) {
    let result;
    try {
      result = arg0 instanceof Uint8Array;
    } catch (_) {
      result = false;
    }
    const ret = result;
    return ret;
  };
  imports.wbg.__wbg_isArray_a1eab7e0d067391b = function(arg0) {
    const ret = Array.isArray(arg0);
    return ret;
  };
  imports.wbg.__wbg_isSafeInteger_343e2beeeece1bb0 = function(arg0) {
    const ret = Number.isSafeInteger(arg0);
    return ret;
  };
  imports.wbg.__wbg_iterator_9a24c88df860dc65 = function() {
    const ret = Symbol.iterator;
    return ret;
  };
  imports.wbg.__wbg_length_a446193dc22c12f8 = function(arg0) {
    const ret = arg0.length;
    return ret;
  };
  imports.wbg.__wbg_length_e2d2a49132c1b256 = function(arg0) {
    const ret = arg0.length;
    return ret;
  };
  imports.wbg.__wbg_log_0cc1b7768397bcfe = function(arg0, arg1, arg2, arg3, arg4, arg5, arg6, arg7) {
    let deferred0_0;
    let deferred0_1;
    try {
      deferred0_0 = arg0;
      deferred0_1 = arg1;
      console.log(getStringFromWasm0(arg0, arg1), getStringFromWasm0(arg2, arg3), getStringFromWasm0(arg4, arg5), getStringFromWasm0(arg6, arg7));
    } finally {
      wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
    }
  };
  imports.wbg.__wbg_log_cb9e190acc5753fb = function(arg0, arg1) {
    let deferred0_0;
    let deferred0_1;
    try {
      deferred0_0 = arg0;
      deferred0_1 = arg1;
      console.log(getStringFromWasm0(arg0, arg1));
    } finally {
      wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
    }
  };
  imports.wbg.__wbg_log_f665cabe523a97df = function(arg0, arg1) {
    console.log(getStringFromWasm0(arg0, arg1));
  };
  imports.wbg.__wbg_lorocounter_new = function(arg0) {
    const ret = LoroCounter.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_lorolist_new = function(arg0) {
    const ret = LoroList.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_loromap_new = function(arg0) {
    const ret = LoroMap.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_loromovablelist_new = function(arg0) {
    const ret = LoroMovableList.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_lorotext_new = function(arg0) {
    const ret = LoroText.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_lorotree_new = function(arg0) {
    const ret = LoroTree.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_lorotreenode_new = function(arg0) {
    const ret = LoroTreeNode.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_mark_7438147ce31e9d4b = function(arg0, arg1) {
    performance.mark(getStringFromWasm0(arg0, arg1));
  };
  imports.wbg.__wbg_measure_fb7825c11612c823 = function() {
    return handleError(function(arg0, arg1, arg2, arg3) {
      let deferred0_0;
      let deferred0_1;
      let deferred1_0;
      let deferred1_1;
      try {
        deferred0_0 = arg0;
        deferred0_1 = arg1;
        deferred1_0 = arg2;
        deferred1_1 = arg3;
        performance.measure(getStringFromWasm0(arg0, arg1), getStringFromWasm0(arg2, arg3));
      } finally {
        wasm.__wbindgen_free(deferred0_0, deferred0_1, 1);
        wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
      }
    }, arguments);
  };
  imports.wbg.__wbg_msCrypto_a61aeb35a24c1329 = function(arg0) {
    const ret = arg0.msCrypto;
    return ret;
  };
  imports.wbg.__wbg_new_405e22f390576ce2 = function() {
    const ret = new Object();
    return ret;
  };
  imports.wbg.__wbg_new_5e0be73521bc8c17 = function() {
    const ret = /* @__PURE__ */ new Map();
    return ret;
  };
  imports.wbg.__wbg_new_78feb108b6472713 = function() {
    const ret = new Array();
    return ret;
  };
  imports.wbg.__wbg_new_a12002a7f91c75be = function(arg0) {
    const ret = new Uint8Array(arg0);
    return ret;
  };
  imports.wbg.__wbg_new_c68d7209be747379 = function(arg0, arg1) {
    const ret = new Error(getStringFromWasm0(arg0, arg1));
    return ret;
  };
  imports.wbg.__wbg_newnoargs_105ed471475aaf50 = function(arg0, arg1) {
    const ret = new Function(getStringFromWasm0(arg0, arg1));
    return ret;
  };
  imports.wbg.__wbg_newwithbyteoffsetandlength_d97e637ebe145a9a = function(arg0, arg1, arg2) {
    const ret = new Uint8Array(arg0, arg1 >>> 0, arg2 >>> 0);
    return ret;
  };
  imports.wbg.__wbg_newwithlength_a381634e90c276d4 = function(arg0) {
    const ret = new Uint8Array(arg0 >>> 0);
    return ret;
  };
  imports.wbg.__wbg_newwithlength_c4c419ef0bc8a1f8 = function(arg0) {
    const ret = new Array(arg0 >>> 0);
    return ret;
  };
  imports.wbg.__wbg_next_25feadfc0913fea9 = function(arg0) {
    const ret = arg0.next;
    return ret;
  };
  imports.wbg.__wbg_next_6574e1a8a62d1055 = function() {
    return handleError(function(arg0) {
      const ret = arg0.next();
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_node_905d3e251edff8a2 = function(arg0) {
    const ret = arg0.node;
    return ret;
  };
  imports.wbg.__wbg_now_454cbad00d40f177 = function() {
    const ret = Date.now();
    return ret;
  };
  imports.wbg.__wbg_ownKeys_3930041068756f1f = function() {
    return handleError(function(arg0) {
      const ret = Reflect.ownKeys(arg0);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_process_dc0fbacc7c1c06f7 = function(arg0) {
    const ret = arg0.process;
    return ret;
  };
  imports.wbg.__wbg_push_737cfc8c1432c2c6 = function(arg0, arg1) {
    const ret = arg0.push(arg1);
    return ret;
  };
  imports.wbg.__wbg_randomFillSync_ac0988aba3254290 = function() {
    return handleError(function(arg0, arg1) {
      arg0.randomFillSync(arg1);
    }, arguments);
  };
  imports.wbg.__wbg_require_60cc747a6bc5215a = function() {
    return handleError(function() {
      const ret = module.require;
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_resolve_4851785c9c5f573d = function(arg0) {
    const ret = Promise.resolve(arg0);
    return ret;
  };
  imports.wbg.__wbg_set_37837023f3d740e8 = function(arg0, arg1, arg2) {
    arg0[arg1 >>> 0] = arg2;
  };
  imports.wbg.__wbg_set_3f1d0b984ed272ed = function(arg0, arg1, arg2) {
    arg0[arg1] = arg2;
  };
  imports.wbg.__wbg_set_65595bdd868b3009 = function(arg0, arg1, arg2) {
    arg0.set(arg1, arg2 >>> 0);
  };
  imports.wbg.__wbg_set_8fc6bf8a5b1071d1 = function(arg0, arg1, arg2) {
    const ret = arg0.set(arg1, arg2);
    return ret;
  };
  imports.wbg.__wbg_set_bb8cecf6a62b9f46 = function() {
    return handleError(function(arg0, arg1, arg2) {
      const ret = Reflect.set(arg0, arg1, arg2);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_setindex_dcd71eabf405bde1 = function(arg0, arg1, arg2) {
    arg0[arg1 >>> 0] = arg2;
  };
  imports.wbg.__wbg_stateBinary_9e1da8a4daa74bc6 = function(arg0, arg1) {
    const ret = stateBinary(getArrayU8FromWasm0(arg0, arg1));
    return ret;
  };
  imports.wbg.__wbg_stateIndex_1fbac6394e58cff9 = function() {
    return handleError(function(arg0, arg1, arg2) {
      stateIndex(arg0, arg1 >>> 0, arg2);
    }, arguments);
  };
  imports.wbg.__wbg_stateOptions_dcb483514d1b925d = function() {
    return handleError(function(arg0, arg1) {
      const ret = stateOptions(arg0, arg1 !== 0);
      return ret;
    }, arguments);
  };
  imports.wbg.__wbg_stateRootCid_9198769cdd9f37d5 = function(arg0, arg1, arg2) {
    const ret = stateRootCid(getStringFromWasm0(arg0, arg1), arg2);
    return ret;
  };
  imports.wbg.__wbg_stateSet_a3e74b27d3d26f9d = function() {
    return handleError(function(arg0, arg1, arg2) {
      stateSet(arg0, arg1, arg2);
    }, arguments);
  };
  imports.wbg.__wbg_stateSlice_aafffe8ecba122b8 = function(arg0, arg1, arg2) {
    const ret = stateSlice(arg0, arg1 >>> 0, arg2 >>> 0);
    return ret;
  };
  imports.wbg.__wbg_static_accessor_GLOBAL_88a902d13a557d07 = function() {
    const ret = typeof global === "undefined" ? null : global;
    return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
  };
  imports.wbg.__wbg_static_accessor_GLOBAL_THIS_56578be7e9f832b0 = function() {
    const ret = typeof globalThis === "undefined" ? null : globalThis;
    return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
  };
  imports.wbg.__wbg_static_accessor_SELF_37c5d418e4bf5819 = function() {
    const ret = typeof self === "undefined" ? null : self;
    return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
  };
  imports.wbg.__wbg_static_accessor_WINDOW_5de37043a91a9c40 = function() {
    const ret = typeof window === "undefined" ? null : window;
    return isLikeNone(ret) ? 0 : addToExternrefTable0(ret);
  };
  imports.wbg.__wbg_subarray_aa9065fa9dc5df96 = function(arg0, arg1, arg2) {
    const ret = arg0.subarray(arg1 >>> 0, arg2 >>> 0);
    return ret;
  };
  imports.wbg.__wbg_then_44b73946d2fb3e7d = function(arg0, arg1) {
    const ret = arg0.then(arg1);
    return ret;
  };
  imports.wbg.__wbg_value_cd1ffa7b1ab794f1 = function(arg0) {
    const ret = arg0.value;
    return ret;
  };
  imports.wbg.__wbg_versions_c01dfd4722a88165 = function(arg0) {
    const ret = arg0.versions;
    return ret;
  };
  imports.wbg.__wbg_versionvector_new = function(arg0) {
    const ret = VersionVector.__wrap(arg0);
    return ret;
  };
  imports.wbg.__wbg_warn_52b319b1b6be3d2f = function(arg0, arg1) {
    console.warn(getStringFromWasm0(arg0, arg1));
  };
  imports.wbg.__wbindgen_as_number = function(arg0) {
    const ret = +arg0;
    return ret;
  };
  imports.wbg.__wbindgen_bigint_from_i64 = function(arg0) {
    const ret = arg0;
    return ret;
  };
  imports.wbg.__wbindgen_bigint_from_u64 = function(arg0) {
    const ret = BigInt.asUintN(64, arg0);
    return ret;
  };
  imports.wbg.__wbindgen_bigint_get_as_i64 = function(arg0, arg1) {
    const v = arg1;
    const ret = typeof v === "bigint" ? v : void 0;
    getDataViewMemory0().setBigInt64(arg0 + 8 * 1, isLikeNone(ret) ? BigInt(0) : ret, true);
    getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
  };
  imports.wbg.__wbindgen_boolean_get = function(arg0) {
    const v = arg0;
    const ret = typeof v === "boolean" ? v ? 1 : 0 : 2;
    return ret;
  };
  imports.wbg.__wbindgen_cb_drop = function(arg0) {
    const obj = arg0.original;
    if (obj.cnt-- == 1) {
      obj.a = 0;
      return true;
    }
    const ret = false;
    return ret;
  };
  imports.wbg.__wbindgen_closure_wrapper1226 = function(arg0, arg1, arg2) {
    const ret = makeMutClosure(arg0, arg1, 160, __wbg_adapter_60);
    return ret;
  };
  imports.wbg.__wbindgen_closure_wrapper1228 = function(arg0, arg1, arg2) {
    const ret = makeMutClosure(arg0, arg1, 160, __wbg_adapter_63);
    return ret;
  };
  imports.wbg.__wbindgen_debug_string = function(arg0, arg1) {
    const ret = debugString(arg1);
    const ptr1 = passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len1 = WASM_VECTOR_LEN;
    getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
    getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
  };
  imports.wbg.__wbindgen_error_new = function(arg0, arg1) {
    const ret = new Error(getStringFromWasm0(arg0, arg1));
    return ret;
  };
  imports.wbg.__wbindgen_in = function(arg0, arg1) {
    const ret = arg0 in arg1;
    return ret;
  };
  imports.wbg.__wbindgen_init_externref_table = function() {
    const table = wasm.__wbindgen_export_4;
    const offset = table.grow(4);
    table.set(0, void 0);
    table.set(offset + 0, void 0);
    table.set(offset + 1, null);
    table.set(offset + 2, true);
    table.set(offset + 3, false);
    ;
  };
  imports.wbg.__wbindgen_is_array = function(arg0) {
    const ret = Array.isArray(arg0);
    return ret;
  };
  imports.wbg.__wbindgen_is_bigint = function(arg0) {
    const ret = typeof arg0 === "bigint";
    return ret;
  };
  imports.wbg.__wbindgen_is_function = function(arg0) {
    const ret = typeof arg0 === "function";
    return ret;
  };
  imports.wbg.__wbindgen_is_null = function(arg0) {
    const ret = arg0 === null;
    return ret;
  };
  imports.wbg.__wbindgen_is_object = function(arg0) {
    const val = arg0;
    const ret = typeof val === "object" && val !== null;
    return ret;
  };
  imports.wbg.__wbindgen_is_string = function(arg0) {
    const ret = typeof arg0 === "string";
    return ret;
  };
  imports.wbg.__wbindgen_is_undefined = function(arg0) {
    const ret = arg0 === void 0;
    return ret;
  };
  imports.wbg.__wbindgen_jsval_eq = function(arg0, arg1) {
    const ret = arg0 === arg1;
    return ret;
  };
  imports.wbg.__wbindgen_jsval_loose_eq = function(arg0, arg1) {
    const ret = arg0 == arg1;
    return ret;
  };
  imports.wbg.__wbindgen_memory = function() {
    const ret = wasm.memory;
    return ret;
  };
  imports.wbg.__wbindgen_number_get = function(arg0, arg1) {
    const obj = arg1;
    const ret = typeof obj === "number" ? obj : void 0;
    getDataViewMemory0().setFloat64(arg0 + 8 * 1, isLikeNone(ret) ? 0 : ret, true);
    getDataViewMemory0().setInt32(arg0 + 4 * 0, !isLikeNone(ret), true);
  };
  imports.wbg.__wbindgen_number_new = function(arg0) {
    const ret = arg0;
    return ret;
  };
  imports.wbg.__wbindgen_rethrow = function(arg0) {
    throw arg0;
  };
  imports.wbg.__wbindgen_string_get = function(arg0, arg1) {
    const obj = arg1;
    const ret = typeof obj === "string" ? obj : void 0;
    var ptr1 = isLikeNone(ret) ? 0 : passStringToWasm0(ret, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    var len1 = WASM_VECTOR_LEN;
    getDataViewMemory0().setInt32(arg0 + 4 * 1, len1, true);
    getDataViewMemory0().setInt32(arg0 + 4 * 0, ptr1, true);
  };
  imports.wbg.__wbindgen_string_new = function(arg0, arg1) {
    const ret = getStringFromWasm0(arg0, arg1);
    return ret;
  };
  imports.wbg.__wbindgen_throw = function(arg0, arg1) {
    throw new Error(getStringFromWasm0(arg0, arg1));
  };
  imports.wbg.__wbindgen_typeof = function(arg0) {
    const ret = typeof arg0;
    return ret;
  };
  imports["./snippets/loro-wasm-704a44faa43c032c/inline0.js"] = inline0_exports;
  return imports;
}
__name(__wbg_get_imports, "__wbg_get_imports");
function __wbg_init_memory(imports, memory) {
}
__name(__wbg_init_memory, "__wbg_init_memory");
function __wbg_finalize_init(instance, module3) {
  wasm = instance.exports;
  __wbg_init.__wbindgen_wasm_module = module3;
  cachedDataViewMemory0 = null;
  cachedUint8ArrayMemory0 = null;
  wasm.__wbindgen_start();
  return wasm;
}
__name(__wbg_finalize_init, "__wbg_finalize_init");
function initSync(module3) {
  if (wasm !== void 0) return wasm;
  if (typeof module3 !== "undefined") {
    if (Object.getPrototypeOf(module3) === Object.prototype) {
      ({ module: module3 } = module3);
    } else {
      console.warn("using deprecated parameters for `initSync()`; pass a single object instead");
    }
  }
  const imports = __wbg_get_imports();
  __wbg_init_memory(imports);
  if (!(module3 instanceof WebAssembly.Module)) {
    module3 = new WebAssembly.Module(module3);
  }
  const instance = new WebAssembly.Instance(module3, imports);
  return __wbg_finalize_init(instance, module3);
}
__name(initSync, "initSync");
async function __wbg_init(module_or_path) {
  if (wasm !== void 0) return wasm;
  if (typeof module_or_path !== "undefined") {
    if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
      ({ module_or_path } = module_or_path);
    } else {
      console.warn("using deprecated parameters for the initialization function; pass a single object instead");
    }
  }
  if (typeof module_or_path === "undefined") {
    module_or_path = new URL("loro_wasm_bg.wasm", import.meta.url);
  }
  const imports = __wbg_get_imports();
  if (typeof module_or_path === "string" || typeof Request === "function" && module_or_path instanceof Request || typeof URL === "function" && module_or_path instanceof URL) {
    module_or_path = fetch(module_or_path);
  }
  __wbg_init_memory(imports);
  const { instance, module: module3 } = await __wbg_load(await module_or_path, imports);
  return __wbg_finalize_init(instance, module3);
}
__name(__wbg_init, "__wbg_init");
var __loroContainerIdCache = /* @__PURE__ */ Symbol("loroContainerId");
function __loroCacheContainerIdGetter(ContainerClass) {
  const prototype = ContainerClass.prototype;
  const idDescriptor = Object.getOwnPropertyDescriptor(prototype, "id");
  const getId = idDescriptor && idDescriptor.get;
  const destroyDescriptor = Object.getOwnPropertyDescriptor(
    prototype,
    "__destroy_into_raw"
  );
  const destroy = destroyDescriptor && destroyDescriptor.value;
  if (!getId || !destroyDescriptor || !destroy) {
    throw new Error("Unexpected wasm-bindgen container wrapper shape");
  }
  Object.defineProperty(prototype, "id", {
    ...idDescriptor,
    get() {
      if (this.__wbg_ptr === 0) {
        return getId.call(this);
      }
      const cached = this[__loroContainerIdCache];
      if (cached !== void 0) {
        return cached;
      }
      const id = getId.call(this);
      try {
        Object.defineProperty(this, __loroContainerIdCache, {
          configurable: true,
          value: id
        });
      } catch {
      }
      return id;
    }
  });
  Object.defineProperty(prototype, "__destroy_into_raw", {
    ...destroyDescriptor,
    value() {
      try {
        delete this[__loroContainerIdCache];
      } catch {
      }
      return destroy.call(this);
    }
  });
}
__name(__loroCacheContainerIdGetter, "__loroCacheContainerIdGetter");
function __loroCacheKindMethod(ContainerClass) {
  const prototype = ContainerClass.prototype;
  const kindDescriptor = Object.getOwnPropertyDescriptor(prototype, "kind");
  const kind = kindDescriptor && kindDescriptor.value;
  if (typeof kind !== "function") {
    throw new Error("Unexpected wasm-bindgen container wrapper shape");
  }
  let cached;
  Object.defineProperty(prototype, "kind", {
    ...kindDescriptor,
    value() {
      if (this.__wbg_ptr === 0) {
        return kind.call(this);
      }
      if (cached === void 0) {
        cached = kind.call(this);
      }
      return cached;
    }
  });
}
__name(__loroCacheKindMethod, "__loroCacheKindMethod");
for (const ContainerClass of [
  LoroMap,
  LoroText,
  LoroList,
  LoroTree,
  LoroMovableList,
  LoroCounter
]) {
  __loroCacheContainerIdGetter(ContainerClass);
  __loroCacheKindMethod(ContainerClass);
}

// node_modules/loro-crdt/web/index.js
var CONTAINER_TYPES = [
  "Map",
  "Text",
  "List",
  "Tree",
  "MovableList",
  "Counter"
];
function isContainerId(s) {
  return isValidContainerId(s);
}
__name(isContainerId, "isContainerId");
function isContainer(value) {
  if (typeof value !== "object" || value == null) {
    return false;
  }
  const p = Object.getPrototypeOf(value);
  if (p == null || typeof p !== "object" || typeof p["kind"] !== "function") {
    return false;
  }
  return CONTAINER_TYPES.includes(value.kind());
}
__name(isContainer, "isContainer");
var EphemeralStore = class {
  static {
    __name(this, "EphemeralStore");
  }
  constructor(timeout = 3e4) {
    this.inner = new EphemeralStoreWasm(timeout);
    this.timeout = timeout;
  }
  apply(bytes) {
    this.inner.apply(bytes);
    this.startTimerIfNotEmpty();
  }
  set(key, value) {
    this.inner.set(key, value);
    this.startTimerIfNotEmpty();
  }
  delete(key) {
    this.inner.delete(key);
  }
  get(key) {
    return this.inner.get(key);
  }
  getAllStates() {
    return this.inner.getAllStates();
  }
  encode(key) {
    return this.inner.encode(key);
  }
  encodeAll() {
    return this.inner.encodeAll();
  }
  keys() {
    return this.inner.keys();
  }
  destroy() {
    clearInterval(this.timer);
  }
  subscribe(listener) {
    return this.inner.subscribe(listener);
  }
  subscribeLocalUpdates(listener) {
    return this.inner.subscribeLocalUpdates(listener);
  }
  startTimerIfNotEmpty() {
    if (this.inner.isEmpty() || this.timer != null) {
      return;
    }
    this.timer = setInterval(() => {
      this.inner.removeOutdated();
      if (this.inner.isEmpty()) {
        clearInterval(this.timer);
        this.timer = void 0;
      }
    }, this.timeout / 2);
  }
};
LoroDoc.prototype.toJsonWithReplacer = function(replacer) {
  const processed = /* @__PURE__ */ new Set();
  const doc = this;
  const m = /* @__PURE__ */ __name((key, value) => {
    if (typeof value === "string") {
      if (isContainerId(value) && !processed.has(value)) {
        processed.add(value);
        const container = doc.getContainerById(value);
        if (container == null) {
          throw new Error(`ContainerID not found: ${value}`);
        }
        const ans2 = replacer(key, container);
        if (ans2 === container) {
          const ans3 = container.getShallowValue();
          if (typeof ans3 === "object") {
            return run2(ans3);
          }
          return ans3;
        }
        if (isContainer(ans2)) {
          throw new Error("Using new container is not allowed in toJsonWithReplacer");
        }
        if (typeof ans2 === "object" && ans2 != null) {
          return run2(ans2);
        }
        return ans2;
      }
    }
    if (typeof value === "object" && value != null) {
      return run2(value);
    }
    const ans = replacer(key, value);
    if (isContainer(ans)) {
      throw new Error("Using new container is not allowed in toJsonWithReplacer");
    }
    return ans;
  }, "m");
  const run2 = /* @__PURE__ */ __name((layer2) => {
    if (Array.isArray(layer2)) {
      return layer2.map((item, index) => {
        return m(index, item);
      }).filter((item) => item !== void 0);
    }
    const result = {};
    for (const [key, value] of Object.entries(layer2)) {
      const ans = m(key, value);
      if (ans !== void 0) {
        result[key] = ans;
      }
    }
    return result;
  }, "run");
  const layer = doc.getShallowValue();
  return run2(layer);
};
var CALL_PENDING_EVENTS_WRAPPED = /* @__PURE__ */ Symbol("loro.callPendingEventsWrapped");
function decorateMethod(prototype, method) {
  const descriptor = Object.getOwnPropertyDescriptor(prototype, method);
  if (!descriptor || typeof descriptor.value !== "function") {
    return;
  }
  const original = descriptor.value;
  if (original[CALL_PENDING_EVENTS_WRAPPED]) {
    return;
  }
  const wrapped = /* @__PURE__ */ __name(function(...args) {
    let result;
    try {
      result = original.apply(this, args);
      return result;
    } finally {
      if (result && typeof result.then === "function") {
        result.finally(() => {
          callPendingEvents();
        });
      } else {
        callPendingEvents();
      }
    }
  }, "wrapped");
  wrapped[CALL_PENDING_EVENTS_WRAPPED] = true;
  Object.defineProperty(prototype, method, {
    ...descriptor,
    value: wrapped
  });
}
__name(decorateMethod, "decorateMethod");
function decorateMethods(prototype, methods) {
  for (const method of methods) {
    decorateMethod(prototype, method);
  }
}
__name(decorateMethods, "decorateMethods");
decorateMethods(LoroDoc.prototype, [
  "setDetachedEditing",
  "attach",
  "detach",
  "fork",
  "forkAt",
  "checkoutToLatest",
  "checkout",
  "commit",
  "getCursorPos",
  "revertTo",
  "export",
  "exportJsonUpdates",
  "exportJsonInIdSpan",
  "importJsonUpdates",
  "import",
  "importUpdateBatch",
  "importBatch",
  "travelChangeAncestors",
  "getChangedContainersIn",
  "diff",
  "applyDiff",
  "setPeerId"
]);
decorateMethods(EphemeralStoreWasm.prototype, [
  "set",
  "delete",
  "apply",
  "removeOutdated"
]);
decorateMethods(UndoManager.prototype, ["undo", "redo"]);

// src/loro-runtime.ts
import module2 from "./2f0f4e9e57402ed68e2f85508ca0967828e35738-loro_wasm_bg.wasm";
initSync({ module: module2 });

// node_modules/loro-protocol/dist/index.js
var MAX_MESSAGE_SIZE = 256 * 1024;
var CrdtType = {
  Loro: "%LOR",
  LoroEphemeralStore: "%EPH",
  LoroEphemeralStorePersisted: "%EPS",
  Yjs: "%YJS",
  YjsAwareness: "%YAW",
  Elo: "%ELO",
  Flock: "%FLO"
};
var MessageType = {
  JoinRequest: 0,
  JoinResponseOk: 1,
  JoinError: 2,
  DocUpdate: 3,
  DocUpdateFragmentHeader: 4,
  DocUpdateFragment: 5,
  RoomError: 6,
  Leave: 7,
  Ack: 8
};
var JoinErrorCode = {
  Unknown: 0,
  VersionUnknown: 1,
  AuthFailed: 2,
  AppError: 127
};
var RoomErrorCode = {
  RejoinSuggested: 1,
  Evicted: 2,
  Unknown: 127
};
var UpdateStatusCode = {
  Ok: 0,
  Unknown: 1,
  PermissionDenied: 3,
  InvalidUpdate: 4,
  PayloadTooLarge: 5,
  RateLimited: 6,
  FragmentTimeout: 7,
  AppError: 127
};
function hexToBytes(hex) {
  const hexWithoutPrefix = hex.startsWith("0x") ? hex.slice(2) : hex;
  const bytes = new Uint8Array(hexWithoutPrefix.length / 2);
  for (let i = 0; i < hexWithoutPrefix.length; i += 2) bytes[i / 2] = parseInt(hexWithoutPrefix.slice(i, i + 2), 16);
  return bytes;
}
__name(hexToBytes, "hexToBytes");
function bytesToHex(bytes) {
  let hex = "";
  for (let i = 0; i < bytes.length; i++) hex += bytes[i].toString(16).padStart(2, "0");
  return `0x${hex}`;
}
__name(bytesToHex, "bytesToHex");
var BytesWriter = class {
  static {
    __name(this, "BytesWriter");
  }
  constructor() {
    this.buf = new Uint8Array(32);
    this.len = 0;
  }
  reserve(size) {
    if (this.len + size <= this.buf.length) return;
    let nextLen = nextPow2u32(size + this.len);
    const buf = this.buf;
    this.buf = new Uint8Array(nextLen);
    this.buf.set(buf, 0);
  }
  pushBytes(bytes) {
    if (bytes.length === 0) return;
    this.reserve(bytes.length);
    this.buf.set(bytes, this.len);
    this.len += bytes.length;
  }
  pushByte(byte) {
    this.reserve(1);
    this.buf[this.len] = byte;
    this.len += 1;
  }
  pushUleb128(n) {
    do {
      this.pushByte(n & 127 | (n >= 128 ? 128 : 0));
      n >>>= 7;
    } while (n > 0);
  }
  pushVarBytes(bytes) {
    this.pushUleb128(bytes.length);
    this.pushBytes(bytes);
  }
  pushVarString(str) {
    const bytes = new TextEncoder().encode(str);
    this.pushVarBytes(bytes);
  }
  finalize() {
    return this.buf.slice(0, this.len);
  }
  get length() {
    return this.len;
  }
};
var BytesReader = class {
  static {
    __name(this, "BytesReader");
  }
  constructor(buf) {
    this.offset = 0;
    this.buf = buf;
  }
  get remaining() {
    return this.buf.length - this.offset;
  }
  get position() {
    return this.offset;
  }
  readByte() {
    if (this.offset >= this.buf.length) throw new Error("readByte out of bounds");
    const v = this.buf[this.offset];
    this.offset += 1;
    return v;
  }
  readBytes(len) {
    if (len < 0 || this.offset + len > this.buf.length) throw new Error("readBytes out of bounds");
    const out = this.buf.subarray(this.offset, this.offset + len);
    this.offset += len;
    return out;
  }
  readUleb128() {
    let result = 0;
    let shift = 0;
    while (true) {
      const byte = this.readByte();
      result |= (byte & 127) << shift;
      if ((byte & 128) === 0) break;
      shift += 7;
      if (shift > 35) throw new Error("uleb128 too large");
    }
    return result >>> 0;
  }
  readVarBytes() {
    const len = this.readUleb128();
    return this.readBytes(len);
  }
  readVarString() {
    const bytes = this.readVarBytes();
    return new TextDecoder().decode(bytes);
  }
};
function nextPow2u32(n) {
  let x = n >>> 0;
  x |= x >>> 1;
  x |= x >>> 2;
  x |= x >>> 4;
  x |= x >>> 8;
  x |= x >>> 16;
  x = x + 1 >>> 0;
  return x === 0 ? 4294967296 : x;
}
__name(nextPow2u32, "nextPow2u32");
var validCrdtTypes = [
  CrdtType.Loro,
  CrdtType.LoroEphemeralStore,
  CrdtType.LoroEphemeralStorePersisted,
  CrdtType.Yjs,
  CrdtType.YjsAwareness,
  CrdtType.Elo,
  CrdtType.Flock
];
var MAX_ROOM_ID_LENGTH = 128;
function isCrdtType(x) {
  return typeof x === "string" && (x === CrdtType.Loro || x === CrdtType.LoroEphemeralStore || x === CrdtType.LoroEphemeralStorePersisted || x === CrdtType.Yjs || x === CrdtType.YjsAwareness || x === CrdtType.Flock || x === CrdtType.Elo);
}
__name(isCrdtType, "isCrdtType");
function isMessageType(x) {
  return x === MessageType.JoinRequest || x === MessageType.JoinResponseOk || x === MessageType.JoinError || x === MessageType.DocUpdate || x === MessageType.DocUpdateFragmentHeader || x === MessageType.DocUpdateFragment || x === MessageType.RoomError || x === MessageType.Leave || x === MessageType.Ack;
}
__name(isMessageType, "isMessageType");
function isJoinErrorCode(x) {
  return x === JoinErrorCode.Unknown || x === JoinErrorCode.VersionUnknown || x === JoinErrorCode.AuthFailed || x === JoinErrorCode.AppError;
}
__name(isJoinErrorCode, "isJoinErrorCode");
function isUpdateStatusCode(x) {
  return x === UpdateStatusCode.Ok || x === UpdateStatusCode.Unknown || x === UpdateStatusCode.PermissionDenied || x === UpdateStatusCode.InvalidUpdate || x === UpdateStatusCode.PayloadTooLarge || x === UpdateStatusCode.RateLimited || x === UpdateStatusCode.FragmentTimeout || x === UpdateStatusCode.AppError;
}
__name(isUpdateStatusCode, "isUpdateStatusCode");
function isRoomErrorCode(x) {
  return x === RoomErrorCode.RejoinSuggested || x === RoomErrorCode.Evicted || x === RoomErrorCode.Unknown;
}
__name(isRoomErrorCode, "isRoomErrorCode");
function readBatchId(reader, context) {
  if (reader.remaining < 8) throw new Error(`Invalid ${context}: missing batch ID`);
  const batchIdBytes = reader.readBytes(8);
  if (batchIdBytes.byteLength !== 8) throw new Error(`Invalid ${context}: batch ID must be exactly 8 bytes`);
  return bytesToHex(batchIdBytes);
}
__name(readBatchId, "readBatchId");
function ensureFullyConsumed(reader, context) {
  if (reader.remaining !== 0) throw new Error(`Invalid ${context}: unexpected trailing bytes`);
}
__name(ensureFullyConsumed, "ensureFullyConsumed");
function encode(message) {
  const writer = new BytesWriter();
  if (!validCrdtTypes.includes(message.crdt)) throw new Error(`Invalid CRDT type: ${message.crdt}`);
  const crdtBytes = new TextEncoder().encode(message.crdt);
  writer.pushBytes(crdtBytes);
  const roomIdBytes = new TextEncoder().encode(message.roomId);
  if (roomIdBytes.byteLength > MAX_ROOM_ID_LENGTH) throw new Error("Room ID too long");
  writer.pushVarBytes(roomIdBytes);
  writer.pushByte(message.type);
  switch (message.type) {
    case MessageType.JoinRequest:
      writer.pushVarBytes(message.auth);
      writer.pushVarBytes(message.version);
      break;
    case MessageType.JoinResponseOk:
      writer.pushVarString(message.permission);
      writer.pushVarBytes(message.version);
      if (message.extra) writer.pushVarBytes(message.extra);
      else writer.pushVarBytes(new Uint8Array(0));
      break;
    case MessageType.JoinError:
      writer.pushByte(message.code);
      writer.pushVarString(message.message);
      if (message.code === JoinErrorCode.VersionUnknown && message.receiverVersion) writer.pushVarBytes(message.receiverVersion);
      if (message.code === JoinErrorCode.AppError && message.appCode) writer.pushVarString(message.appCode);
      break;
    case MessageType.DocUpdate:
      writer.pushUleb128(message.updates.length);
      for (const update of message.updates) writer.pushVarBytes(update);
      writer.pushBytes(hexToBytes(message.batchId));
      break;
    case MessageType.DocUpdateFragmentHeader: {
      const batchIdBytes = hexToBytes(message.batchId);
      writer.pushBytes(batchIdBytes);
      writer.pushUleb128(message.fragmentCount);
      writer.pushUleb128(message.totalSizeBytes);
      break;
    }
    case MessageType.DocUpdateFragment: {
      const batchIdBytes = hexToBytes(message.batchId);
      writer.pushBytes(batchIdBytes);
      writer.pushUleb128(message.index);
      writer.pushVarBytes(message.fragment);
      break;
    }
    case MessageType.RoomError:
      writer.pushByte(message.code);
      writer.pushVarString(message.message);
      break;
    case MessageType.Ack:
      writer.pushBytes(hexToBytes(message.refId));
      writer.pushByte(message.status);
      break;
    case MessageType.Leave:
      break;
    default:
      throw new Error("Unknown message type");
  }
  const result = writer.finalize();
  if (result.length > MAX_MESSAGE_SIZE) throw new Error(`Message size ${result.length} exceeds maximum ${MAX_MESSAGE_SIZE}`);
  return result;
}
__name(encode, "encode");
function decode(data) {
  const reader = new BytesReader(data);
  if (reader.remaining < 4) throw new Error("Invalid message: too short for CRDT type");
  const crdtBytes = reader.readBytes(4);
  const crdtMaybe = new TextDecoder().decode(crdtBytes);
  if (!isCrdtType(crdtMaybe)) throw new Error(`Invalid CRDT type: ${crdtMaybe}`);
  const crdt = crdtMaybe;
  if (!validCrdtTypes.includes(crdt)) throw new Error(`Invalid CRDT type: ${crdt}`);
  const roomId = reader.readVarString();
  const typeByte = reader.readByte();
  if (!isMessageType(typeByte)) throw new Error("Invalid message type");
  const type = typeByte;
  switch (type) {
    case MessageType.JoinRequest:
      return {
        type,
        crdt,
        roomId,
        auth: reader.readVarBytes(),
        version: reader.readVarBytes()
      };
    case MessageType.JoinResponseOk: {
      const permissionRaw = reader.readVarString();
      if (permissionRaw !== "read" && permissionRaw !== "write") throw new Error(`Invalid permission: ${permissionRaw}`);
      return {
        type,
        crdt,
        roomId,
        permission: permissionRaw,
        version: reader.readVarBytes(),
        extra: reader.readVarBytes()
      };
    }
    case MessageType.JoinError: {
      const codeByte = reader.readByte();
      const code = isJoinErrorCode(codeByte) ? codeByte : JoinErrorCode.Unknown;
      const message = reader.readVarString();
      let receiverVersion;
      let appCode;
      if (code === JoinErrorCode.VersionUnknown && reader.remaining > 0) receiverVersion = reader.readVarBytes();
      if (code === JoinErrorCode.AppError && reader.remaining > 0) appCode = reader.readVarString();
      return {
        type,
        crdt,
        roomId,
        code,
        message,
        receiverVersion,
        appCode
      };
    }
    case MessageType.DocUpdate: {
      const count = reader.readUleb128();
      const updates = [];
      for (let i = 0; i < count; i++) updates.push(reader.readVarBytes());
      const batchId = readBatchId(reader, "DocUpdate");
      ensureFullyConsumed(reader, "DocUpdate");
      return {
        type,
        crdt,
        roomId,
        updates,
        batchId
      };
    }
    case MessageType.DocUpdateFragmentHeader: {
      const batchId = readBatchId(reader, "DocUpdateFragmentHeader");
      const fragmentCount = reader.readUleb128();
      const totalSizeBytes = reader.readUleb128();
      ensureFullyConsumed(reader, "DocUpdateFragmentHeader");
      return {
        type,
        crdt,
        roomId,
        batchId,
        fragmentCount,
        totalSizeBytes
      };
    }
    case MessageType.DocUpdateFragment: {
      const batchId = readBatchId(reader, "DocUpdateFragment");
      const index = reader.readUleb128();
      const fragment = reader.readVarBytes();
      ensureFullyConsumed(reader, "DocUpdateFragment");
      return {
        type,
        crdt,
        roomId,
        batchId,
        index,
        fragment
      };
    }
    case MessageType.RoomError: {
      const codeByte2 = reader.readByte();
      return {
        type,
        crdt,
        roomId,
        code: isRoomErrorCode(codeByte2) ? codeByte2 : RoomErrorCode.Unknown,
        message: reader.readVarString()
      };
    }
    case MessageType.Ack: {
      if (reader.remaining < 9) throw new Error("Invalid Ack: missing fields");
      const refId = readBatchId(reader, "Ack");
      const statusByte = reader.readByte();
      const status = isUpdateStatusCode(statusByte) ? statusByte : UpdateStatusCode.Unknown;
      ensureFullyConsumed(reader, "Ack");
      return {
        type,
        crdt,
        roomId,
        refId,
        status
      };
    }
    case MessageType.Leave:
      return {
        type,
        crdt,
        roomId
      };
    default:
      throw new Error("Unknown message type");
  }
}
__name(decode, "decode");

// src/session-doc/constants.ts
var MSG_INLINE_MAX = 256 * 1024;
var RETAIN_DAYS = 3;
var COMPACT_LOG_BYTES = 256 * 1024;
var COMPACT_LOG_ROWS = 32;
var SOFT_CEILING_BYTES = 25 * 1024 * 1024;
var DOC_LRU_BYTE_BUDGET = 80 * 1024 * 1024;
var TAIL_MESSAGE_COUNT = 64;
var COMMAND_DEFAULT_TTL_MS = 24 * 60 * 60 * 1e3;
var SESSION_SCHEMA_VERSION = 1;

// src/session-doc/messages.ts
var nonblank = /* @__PURE__ */ __name((value) => typeof value === "string" && value.trim().length > 0, "nonblank");
var isPeerMessageEntry = /* @__PURE__ */ __name((entry) => {
  const peer = entry.peerMessage;
  return entry.role === "user" && entry.continuationOf == null && peer != null && typeof peer === "object" && !Array.isArray(peer) && nonblank(peer.commandId) && peer.commandId === entry.id && nonblank(peer.sourceChatId) && nonblank(peer.threadId) && (peer.replyTo == null || nonblank(peer.replyTo));
}, "isPeerMessageEntry");
var samePeerMessage = /* @__PURE__ */ __name((left, right) => right != null && typeof right === "object" && !Array.isArray(right) && left.commandId === right.commandId && left.sourceChatId === right.sourceChatId && left.threadId === right.threadId && (left.replyTo ?? null) === (right.replyTo ?? null), "samePeerMessage");
var encoder2 = new TextEncoder();
var joinContinuations = /* @__PURE__ */ __name((entries) => {
  if (!entries.some((e) => e.continuationOf)) return entries;
  const rootIndex = /* @__PURE__ */ new Map();
  const order = [];
  const joinedParts = /* @__PURE__ */ new Map();
  for (const entry of entries) {
    if (entry.continuationOf) {
      const at = rootIndex.get(entry.continuationOf);
      if (at !== void 0) {
        const root = order[at];
        if (root.role === entry.role && (!isPeerMessageEntry(root) || samePeerMessage(root.peerMessage, entry.peerMessage))) {
          let parts = joinedParts.get(at);
          if (!parts) {
            parts = [...root.parts];
            joinedParts.set(at, parts);
            order[at] = { ...root, parts };
          }
          for (const part of entry.parts) parts.push(part);
          continue;
        }
      }
      order.push(entry);
      continue;
    }
    rootIndex.set(entry.id, order.length);
    order.push(entry);
  }
  return order;
}, "joinContinuations");

// src/session-doc/tail.ts
var readMessageEntries = /* @__PURE__ */ __name((doc) => {
  const messages = doc.getList("messages");
  try {
    return messages.toJSON();
  } finally {
    messages.free();
  }
}, "readMessageEntries");
var materializeTail = /* @__PURE__ */ __name((doc, now, tailCount = TAIL_MESSAGE_COUNT) => {
  const all = joinContinuations(readMessageEntries(doc));
  const meta = doc.getMap("meta");
  try {
    return {
      chatId: meta.get("chatId") ?? "",
      schemaVersion: meta.get("schemaVersion") ?? SESSION_SCHEMA_VERSION,
      messages: all.slice(-tailCount),
      totalMessages: all.length,
      updatedAt: now
    };
  } finally {
    meta.free();
  }
}, "materializeTail");

// src/blobs.ts
var CHUNK_BYTES = 15e5;
var createBlobStore = /* @__PURE__ */ __name((sql) => {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS blobs (name TEXT NOT NULL, idx INTEGER NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY (name, idx))"
  );
  return {
    put(name, bytes) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
      for (let i = 0, idx = 0; i === 0 || i < bytes.length; i += CHUNK_BYTES, idx++) {
        const chunk = bytes.subarray(i, Math.min(i + CHUNK_BYTES, bytes.length));
        sql.exec("INSERT INTO blobs (name, idx, bytes) VALUES (?, ?, ?)", name, idx, chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength));
      }
    },
    get(name) {
      const rows = [...sql.exec("SELECT bytes FROM blobs WHERE name = ? ORDER BY idx", name)];
      if (rows.length === 0) return void 0;
      const parts = rows.map((r) => new Uint8Array(r.bytes));
      const total = parts.reduce((a, p) => a + p.length, 0);
      const out = new Uint8Array(total);
      let off = 0;
      for (const p of parts) {
        out.set(p, off);
        off += p.length;
      }
      return out;
    },
    byteLength(name) {
      for (const row of sql.exec("SELECT SUM(length(bytes)) AS size FROM blobs WHERE name = ?", name)) {
        return row.size === null ? void 0 : row.size;
      }
      return void 0;
    },
    delete(name) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
    }
  };
}, "createBlobStore");
var textEncoder = new TextEncoder();
var textDecoder = new TextDecoder();
var putJsonBlob = /* @__PURE__ */ __name((store, name, value) => store.put(name, textEncoder.encode(JSON.stringify(value))), "putJsonBlob");
var getJsonBlob = /* @__PURE__ */ __name((store, name) => {
  const bytes = store.get(name);
  if (!bytes) return void 0;
  try {
    return JSON.parse(textDecoder.decode(bytes));
  } catch {
    return void 0;
  }
}, "getJsonBlob");

// src/device-room.ts
var SESSION_READ = "session.read";
var GRANT_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
var parseTrustedDeviceGrant = /* @__PURE__ */ __name((encoded, subject, projectScope, now) => {
  if (!encoded || encoded.length > 16 * 1024) return void 0;
  let value;
  try {
    value = JSON.parse(encoded);
  } catch {
    return void 0;
  }
  if (!value || typeof value !== "object") return void 0;
  if (typeof value.grantId !== "string" || !GRANT_ID_RE.test(value.grantId) || value.subject !== subject || !value.scope || typeof value.scope !== "object" || value.scope.projectId !== projectScope || typeof value.scope.deploymentId !== "string" || !GRANT_ID_RE.test(value.scope.deploymentId) || typeof value.scope.sessionId !== "string" || !GRANT_ID_RE.test(value.scope.sessionId) || !Number.isSafeInteger(value.scope.lifecycleEpoch) || value.scope.lifecycleEpoch < 1 || typeof value.sandboxId !== "string" || !GRANT_ID_RE.test(value.sandboxId) || typeof value.targetDeviceId !== "string" || !GRANT_ID_RE.test(value.targetDeviceId) || !Number.isSafeInteger(value.lifecycleEpoch) || value.lifecycleEpoch !== value.scope.lifecycleEpoch || !Array.isArray(value.capabilities) || value.capabilities.length === 0 || value.capabilities.length > 32 || value.capabilities.some(
    (capability) => typeof capability !== "string" || capability.length === 0 || capability.length > 128
  ) || !Number.isSafeInteger(value.grantedAt) || !Number.isSafeInteger(value.expiresAt) || value.grantedAt > now || value.expiresAt <= now || value.grantedAt >= value.expiresAt || value.revokedAt !== null) {
    return void 0;
  }
  return value;
}, "parseTrustedDeviceGrant");
var canonicalGrantEnvelope = /* @__PURE__ */ __name((value) => ({
  grant: {
    id: value.grantId,
    principalSubject: value.subject,
    scope: value.scope,
    capabilities: value.capabilities,
    sandboxId: value.sandboxId,
    deviceId: value.targetDeviceId,
    lifecycleEpoch: value.lifecycleEpoch,
    grantedBy: "comet-edge-device-room",
    grantedAt: value.grantedAt,
    expiresAt: value.expiresAt,
    revokedAt: value.revokedAt
  },
  roomId: `s4/${value.scope.projectId}/${value.scope.deploymentId}/${value.scope.sessionId}`,
  targetDeviceId: value.targetDeviceId,
  targetSessionId: value.scope.sessionId
}), "canonicalGrantEnvelope");
var parseCapabilities = /* @__PURE__ */ __name((request) => (request.headers.get(AUTH_CAPABILITIES_HEADER) ?? "").split(/\s+/).filter(Boolean), "parseCapabilities");
var hasCapability = /* @__PURE__ */ __name((capabilities, capability) => capabilities.includes(capability), "hasCapability");
var encodeDeviceFrame = /* @__PURE__ */ __name((header, payload) => {
  const writer = new BytesWriter();
  writer.pushVarString(JSON.stringify(header));
  writer.pushBytes(payload);
  return writer.finalize();
}, "encodeDeviceFrame");
var decodeDeviceFrame = /* @__PURE__ */ __name((bytes) => {
  const reader = new BytesReader(bytes);
  const header = JSON.parse(reader.readVarString());
  const payload = reader.readBytes(reader.remaining);
  return { header, payload };
}, "decodeDeviceFrame");
var HOST_TAG = "host";
var clientTag = /* @__PURE__ */ __name((connId) => `client:${connId}`, "clientTag");
var authorizedDeviceSocketRole = /* @__PURE__ */ __name((requestedRole, hasDeviceGrant, hostAuthorization, peerClient = false, peerReply = false) => {
  if (requestedRole === "host") {
    const requiredAuthorization = hasDeviceGrant ? "sandbox" : "local";
    return hostAuthorization === requiredAuthorization ? "host" : void 0;
  }
  if (requestedRole === null || requestedRole === "client") {
    if (peerReply) return hostAuthorization === void 0 ? "client" : void 0;
    if (peerClient) return hasDeviceGrant && hostAuthorization === void 0 ? "client" : void 0;
    return !hasDeviceGrant && hostAuthorization === void 0 ? "client" : void 0;
  }
  return void 0;
}, "authorizedDeviceSocketRole");
var enforceDeviceHostGrantAuthority = /* @__PURE__ */ __name(async (ws, state, now, validate) => {
  let authorized = state?.hostAuthorization === "local" && state.grant === void 0;
  if (state?.hostAuthorization === "sandbox" && state.grant) {
    const { grantId, expiresAt } = state.grant;
    if (GRANT_ID_RE.test(grantId) && Number.isSafeInteger(expiresAt) && expiresAt > now) {
      try {
        authorized = await validate(grantId) === true;
      } catch {
        authorized = false;
      }
    }
  }
  if (authorized) return true;
  try {
    ws.close(4403, "device grant invalid");
  } catch {
  }
  return false;
}, "enforceDeviceHostGrantAuthority");
var deviceGrantTargetsRoom = /* @__PURE__ */ __name((credentialTargetDeviceId, roomTargetDeviceId) => credentialTargetDeviceId === roomTargetDeviceId, "deviceGrantTargetsRoom");
var HOST_LIVENESS_MS = 75e3;
var RELAY_KIND = " relay";
var NUDGE_KIND = "nudge";
var NUDGE_ACK_KIND = "nudge-ack";
var GRANT_KIND = "grant";
var NUDGE_MAX_PENDING = 256;
var CHAT_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
var DeviceRoom = class {
  static {
    __name(this, "DeviceRoom");
  }
  ctx;
  env;
  blobs;
  revokedGrants = /* @__PURE__ */ new Set();
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS pending_nudges (chat_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)"
    );
    const columns = [...ctx.storage.sql.exec("PRAGMA table_info(pending_nudges)")];
    if (!columns.some((column) => column.name === "nudge_id")) {
      ctx.storage.sql.exec("ALTER TABLE pending_nudges ADD COLUMN nudge_id TEXT");
    }
    for (const row of ctx.storage.sql.exec("SELECT chat_id FROM pending_nudges WHERE nudge_id IS NULL")) {
      ctx.storage.sql.exec("UPDATE pending_nudges SET nudge_id = ? WHERE chat_id = ?", crypto.randomUUID(), row.chat_id);
    }
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }
  getMeta(key) {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value;
  }
  setMeta(key, value) {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }
  /** The host socket to route to: the freshest one that has proven itself
   * alive within [`HOST_LIVENESS_MS`]. `undefined` = no live host, which is
   * what makes clients see `host_offline` instead of hanging on a corpse.
   *
   * `exclude` drops the socket a close is being handled for — the runtime
   * still lists it during `webSocketClose`, and counting it as live would
   * suppress the very `host_closed` that close is supposed to announce. */
  liveHost(exclude) {
    return pickLiveHost(
      this.ctx.getWebSockets(HOST_TAG).filter((ws) => {
        const state = ws.deserializeAttachment();
        return state?.durableSync === true && !state.superseded;
      }).map((ws) => ({
        ws,
        // Auto-pongs are stamped even while hibernating; `joinedAt` covers the
        // window before a fresh socket's first ping. Sockets attached by an
        // older deploy have neither and read as ancient — correct: they are.
        lastSeenAt: Math.max(
          this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? 0,
          ws.deserializeAttachment()?.joinedAt ?? 0
        )
      })),
      Date.now(),
      exclude
    );
  }
  /** Most recent non-superseded client for a logical connection id.
   * Some runtimes retain a closing WebSocket, so array order is not a
   * sufficient reconnect policy. */
  liveClient(connId) {
    let latest;
    for (const ws of this.ctx.getWebSockets(clientTag(connId))) {
      const state = ws.deserializeAttachment();
      if (state?.role !== "client" || state.durableSync !== true || state.superseded) continue;
      const joinedAt = state.joinedAt ?? 0;
      if (!latest || joinedAt > latest.joinedAt) latest = { ws, joinedAt };
    }
    return latest?.ws;
  }
  async revokeGrant(grantId) {
    this.revokedGrants.add(grantId);
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment();
      if (state?.grant?.grantId !== grantId) continue;
      try {
        ws.close(4403, "device grant revoked");
      } catch {
      }
    }
  }
  async authorizeHost(ws) {
    const state = ws.deserializeAttachment();
    if (state?.durableSync !== true) {
      ws.close(4406, CREW_UPDATE_REQUIRED);
      return false;
    }
    return enforceDeviceHostGrantAuthority(ws, state, Date.now(), async (grantId) => {
      if (this.revokedGrants.has(grantId)) return false;
      const stub = this.env.AUTH_GRANTS.get(this.env.AUTH_GRANTS.idFromName(grantId));
      const response = await stub.fetch(
        new Request(`https://grant.internal/status?grantId=${encodeURIComponent(grantId)}`, {
          headers: { [GRANT_EVENT_HEADER]: "status" }
        })
      );
      return response.status === 204 && !this.revokedGrants.has(grantId);
    });
  }
  async authorizePeerClient(ws, state) {
    if (!state.peerSessionId) return true;
    if (state.peerReply && !state.grant) return true;
    const grant = state.grant;
    if (!grant || grant.expiresAt <= Date.now() || this.revokedGrants.has(grant.grantId)) {
      ws.close(4403, "device grant invalid");
      return false;
    }
    try {
      const stub = this.env.AUTH_GRANTS.get(this.env.AUTH_GRANTS.idFromName(grant.grantId));
      const response = await stub.fetch(
        new Request(`https://grant.internal/status?grantId=${encodeURIComponent(grant.grantId)}`, {
          headers: { [GRANT_EVENT_HEADER]: "status" }
        })
      );
      if (response.status === 204 && !this.revokedGrants.has(grant.grantId)) return true;
    } catch {
    }
    ws.close(4403, "device grant invalid");
    return false;
  }
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/grant-revoked" && request.method === "POST") {
      if (request.headers.get(GRANT_EVENT_HEADER) !== "revoke") {
        return new Response("forbidden", { status: 403 });
      }
      let body;
      try {
        body = await request.json();
      } catch {
        return new Response("invalid request", { status: 400 });
      }
      if (!body || typeof body !== "object" || !("grantId" in body) || typeof body.grantId !== "string" || !GRANT_ID_RE.test(body.grantId)) {
        return new Response("invalid request", { status: 400 });
      }
      await this.revokeGrant(body.grantId);
      return new Response(null, { status: 204 });
    }
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const projectScope = request.headers.get(AUTH_PROJECT_HEADER);
    const capabilities = parseCapabilities(request);
    if (!projectScope || !hasCapability(capabilities, SESSION_READ)) {
      return new Response("forbidden", { status: 403 });
    }
    const boundScope = this.getMeta("projectScope");
    if (boundScope && boundScope !== projectScope) return new Response("forbidden", { status: 403 });
    const encodedGrant = request.headers.get(AUTH_GRANT_HEADER);
    const grant = parseTrustedDeviceGrant(encodedGrant, userId, projectScope, Date.now());
    if (encodedGrant && !grant) return new Response("invalid grant", { status: 403 });
    const encodedHostAuthorization = request.headers.get(DEVICE_HOST_AUTH_HEADER);
    const hostAuthorization = encodedHostAuthorization === "local" || encodedHostAuthorization === "sandbox" ? encodedHostAuthorization : void 0;
    if (encodedHostAuthorization && !hostAuthorization) {
      return new Response("forbidden", { status: 403 });
    }
    if (grant) {
      if (grant.capabilities.some((capability) => !capabilities.includes(capability))) {
        return new Response("forbidden", { status: 403 });
      }
    }
    const owner = this.getMeta("owner");
    const durableSync = request.headers.get(SYNC_PROTOCOL_HEADER) === DURABLE_SYNC_PROTOCOL;
    if (url.pathname === "/ws") {
      const requestedPeerSessionId = url.searchParams.get("peerSessionId");
      const peerSessionId = requestedPeerSessionId && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(requestedPeerSessionId) ? requestedPeerSessionId.toLowerCase() : void 0;
      const peerReply = url.searchParams.get("purpose") === "peer-reply";
      const role = authorizedDeviceSocketRole(
        url.searchParams.get("role"),
        grant !== void 0,
        hostAuthorization,
        peerSessionId !== void 0 && !peerReply,
        peerReply
      );
      if (!role) return new Response("forbidden", { status: 403 });
      if (grant && role === "host") {
        const targetDeviceId = url.searchParams.get("targetDeviceId");
        if (targetDeviceId !== grant.targetDeviceId) {
          return new Response("forbidden", { status: 403 });
        }
        const boundDevice = this.getMeta("targetDeviceId");
        if (boundDevice && boundDevice !== grant.targetDeviceId) {
          return new Response("forbidden", { status: 403 });
        }
      }
      if (role === "host") {
        if (!hasCapability(capabilities, "session.environment")) {
          return new Response("forbidden", { status: 403 });
        }
        if (hostAuthorization === "local" && this.getMeta("targetDeviceId")) {
          return new Response("forbidden", { status: 403 });
        }
        if (hostAuthorization === "local") {
          const current = this.liveHost()?.deserializeAttachment();
          const principal = this.getMeta("localOwnerUserId") ?? current?.userId;
          if (principal && principal !== userId) return new Response("forbidden", { status: 403 });
        }
        if (owner && owner !== projectScope) return new Response("forbidden", { status: 403 });
      } else if (!owner || owner !== projectScope) {
        return new Response("forbidden", { status: 403 });
      }
      const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
      const controlSessionId = url.searchParams.get("controlSessionId") ?? void 0;
      if ((controlSessionId || url.searchParams.get("purpose") === "control") && (url.searchParams.has("controlDeploymentId") || url.searchParams.has("deploymentId"))) {
        return new Response("scoped_control_not_supported", { status: 403 });
      }
      if (controlSessionId && (role !== "client" || grant || peerSessionId || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(controlSessionId))) {
        return new Response("forbidden", { status: 403 });
      }
      if (!durableSync) {
        return json2({ error: "crew_update_required", message: CREW_UPDATE_REQUIRED }, 426);
      }
      if (!boundScope) this.setMeta("projectScope", projectScope);
      if (role === "host") {
        if (grant && !this.getMeta("targetDeviceId")) this.setMeta("targetDeviceId", grant.targetDeviceId);
        if (hostAuthorization === "local") this.setMeta("localOwnerUserId", userId);
        if (!owner) this.setMeta("owner", projectScope);
      }
      const staleClients = role === "client" ? this.ctx.getWebSockets(clientTag(connId)) : [];
      const pair = new WebSocketPair();
      if (role === "host") {
        this.ctx.acceptWebSocket(pair[1], [HOST_TAG]);
      } else {
        this.ctx.acceptWebSocket(pair[1], [clientTag(connId)]);
      }
      const state = {
        userId,
        projectScope,
        capabilities,
        durableSync: true,
        role,
        connId,
        joinedAt: Date.now(),
        ...role === "host" ? { hostAuthorization } : {},
        ...grant ? { grant } : {},
        ...peerSessionId ? { peerSessionId } : {},
        ...peerReply ? { peerReply } : {},
        targetDeviceId: url.searchParams.get("targetDeviceId") ?? void 0,
        ...controlSessionId ? { controlSessionId } : {}
      };
      pair[1].serializeAttachment(state);
      if (role === "client") {
        for (const stale of staleClients) {
          const staleState = stale.deserializeAttachment();
          if (staleState) stale.serializeAttachment({ ...staleState, superseded: true });
          try {
            stale.close(4409, "superseded by new client connection");
          } catch {
          }
        }
      }
      if (role === "host") {
        if (!await this.authorizeHost(pair[1])) {
          return new Response(null, { status: 101, webSocket: pair[0] });
        }
        for (const stale of this.ctx.getWebSockets(HOST_TAG)) {
          if (stale === pair[1]) continue;
          const staleState = stale.deserializeAttachment();
          if (staleState) stale.serializeAttachment({ ...staleState, superseded: true });
          try {
            stale.close(4409, "superseded by new host connection");
          } catch {
          }
        }
        for (const client of this.ctx.getWebSockets()) {
          const current = client.deserializeAttachment();
          if (current?.role !== "client" || current.superseded) continue;
          client.serializeAttachment({ ...current, superseded: true });
          this.deliver(client, { s: "", k: RELAY_KIND }, encodeRelayError("host_closed"));
          try {
            client.close(1012, "engine reconnected");
          } catch {
          }
        }
        await this.deliverHostStartup(pair[1], grant);
      }
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    const sidecar = url.pathname.match(/^\/sidecar\/([a-z0-9-]{1,64})$/);
    if (sidecar) {
      const name = sidecar[1];
      if (!owner || owner !== projectScope) return json2({ error: "forbidden" }, owner ? 403 : 404);
      if (request.method === "GET") {
        const value = getJsonBlob(this.blobs, `sidecar:${name}`);
        return value === void 0 ? json2({ error: "not_found" }, 404) : json2(value);
      }
      if (request.method === "POST") {
        if (!hasCapability(capabilities, "session.files") && !hasCapability(capabilities, "session.environment")) {
          return json2({ error: "forbidden" }, 403);
        }
        if (!durableSync) return json2({ error: "crew_update_required", message: CREW_UPDATE_REQUIRED }, 426);
        putJsonBlob(this.blobs, `sidecar:${name}`, await request.json());
        return json2({ ok: true });
      }
    }
    if (url.pathname === "/status" && request.method === "GET") {
      if (!owner || owner !== projectScope) return json2({ error: "forbidden" }, owner ? 403 : 404);
      return json2({
        hostConnected: this.liveHost() !== void 0,
        hostSockets: this.ctx.getWebSockets(HOST_TAG).length
      });
    }
    if (url.pathname === "/nudge" && request.method === "POST") {
      if (!owner || owner !== projectScope || !hasCapability(capabilities, "session.control") && !hasCapability(capabilities, "session.chat")) {
        return json2({ error: "forbidden" }, owner ? 403 : 404);
      }
      if (!durableSync) return json2({ error: "crew_update_required", message: CREW_UPDATE_REQUIRED }, 426);
      const body = await request.json().catch(() => null);
      const chatId = body?.chatId;
      if (typeof chatId !== "string" || !CHAT_ID_RE.test(chatId)) return json2({ error: "bad_chat_id" }, 400);
      if (grant && grant.scope.sessionId !== chatId) return json2({ error: "forbidden" }, 403);
      const existing = [...this.ctx.storage.sql.exec("SELECT chat_id FROM pending_nudges WHERE chat_id = ?", chatId)];
      const count = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM pending_nudges")][0]?.n;
      if (existing.length === 0 && count >= NUDGE_MAX_PENDING) {
        return json2({ error: "nudge_capacity", delivered: false, queued: false }, 503);
      }
      const nudgeId = crypto.randomUUID();
      this.ctx.storage.sql.exec(
        "INSERT INTO pending_nudges (chat_id, queued_at, nudge_id) VALUES (?, ?, ?) ON CONFLICT(chat_id) DO UPDATE SET nudge_id = excluded.nudge_id",
        chatId,
        Date.now(),
        nudgeId
      );
      await this.ctx.storage.sync();
      const host = this.liveHost();
      if (host && await this.authorizeHost(host) && this.liveHost() === host) {
        const hostState = host.deserializeAttachment();
        if (!hostState?.grant || hostState.grant.scope.sessionId === chatId) {
          const delivered = this.deliver(
            host,
            { s: chatId, k: NUDGE_KIND },
            new TextEncoder().encode(JSON.stringify({ chatId, nudgeId }))
          );
          return json2({ delivered, queued: true });
        }
      }
      return json2({ delivered: false, queued: true });
    }
    return new Response("not found", { status: 404 });
  }
  replayNudges(host) {
    const rows = [
      ...this.ctx.storage.sql.exec("SELECT chat_id, nudge_id FROM pending_nudges ORDER BY queued_at ASC")
    ];
    if (rows.length === 0) return;
    for (const row of rows) {
      const state = host.deserializeAttachment();
      if (state?.grant && state.grant.scope.sessionId !== row.chat_id) continue;
      this.deliver(
        host,
        { s: row.chat_id, k: NUDGE_KIND },
        new TextEncoder().encode(JSON.stringify({ chatId: row.chat_id, nudgeId: row.nudge_id }))
      );
    }
  }
  async deliverHostStartup(host, grant) {
    await this.ctx.storage.sync();
    if (this.liveHost() !== host || host.deserializeAttachment()?.superseded) return;
    if (grant) {
      this.deliver(
        host,
        { s: grant.scope.sessionId, k: GRANT_KIND },
        new TextEncoder().encode(JSON.stringify(canonicalGrantEnvelope(grant)))
      );
    }
    this.replayNudges(host);
  }
  async webSocketMessage(ws, message) {
    const state = ws.deserializeAttachment();
    if (!state) {
      ws.close(1008, "Missing socket authority");
      return;
    }
    if (state.durableSync !== true) {
      ws.close(4406, CREW_UPDATE_REQUIRED);
      return;
    }
    if (state.role === "client" && !await this.authorizePeerClient(ws, state)) return;
    if (state.role === "host" && !await this.authorizeHost(ws)) return;
    if (ws.deserializeAttachment()?.superseded) return;
    if (typeof message === "string") return;
    let frame;
    try {
      frame = decodeDeviceFrame(new Uint8Array(message));
    } catch {
      ws.close(1002, "Frame error");
      return;
    }
    if (state.role === "client") {
      if (frame.header.k !== "rpc" && frame.header.k !== "term") {
        this.rejectRequest(ws, frame, "reserved_frame_kind");
        return;
      }
      const actorSubject = frame.header.k === "rpc" ? controlActorForRpc(frame.payload) : void 0;
      if (actorSubject !== void 0 && actorSubject !== state.userId) {
        this.rejectRequest(ws, frame, "actor_mismatch");
        return;
      }
      if (!state.peerSessionId) {
        try {
          const value = JSON.parse(new TextDecoder().decode(frame.payload));
          if (value.method === "DeliverPeerMessage") {
            this.rejectRequest(ws, frame, "session_scope_denied");
            return;
          }
        } catch {
        }
      }
      if (state.peerSessionId && (frame.header.k !== "rpc" || (state.peerReply && !state.grant ? !rpcAllowedForDirectPeerReply(frame.payload, state.peerSessionId) : !state.grant || !rpcAllowedForPeerSession(
        frame.payload,
        state.grant.scope.sessionId,
        state.grant.scope.deploymentId,
        state.grant.targetDeviceId,
        state.peerSessionId
      )))) {
        this.rejectRequest(ws, frame, "session_scope_denied");
        return;
      }
      const required = requiredCapabilityForRpc(frame.header, frame.payload);
      if (!hasCapability(state.capabilities, required)) {
        this.rejectRequest(ws, frame, "capability_denied");
        return;
      }
      const host = this.liveHost();
      if (!host || !await this.authorizeHost(host)) {
        this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError("host_offline"));
        return;
      }
      if (ws.deserializeAttachment()?.superseded) return;
      if (this.liveHost() !== host) {
        this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError("host_closed"));
        return;
      }
      const hostState = host.deserializeAttachment();
      const hostGrant = hostState?.grant;
      let method;
      try {
        method = JSON.parse(new TextDecoder().decode(frame.payload)).method;
      } catch {
      }
      if (method === "AdmitPeerCommand" || method === "ReadSessionCommand" && !hostGrant || state.controlSessionId) {
        const current = ws.deserializeAttachment();
        const admission = current && frame.header.k === "rpc" ? peerCommandAdmission(current, hostState, frame.payload, Date.now()) : void 0;
        if (!admission) {
          this.rejectRequest(ws, frame, "peer_command_scope_denied");
          return;
        }
        ws.serializeAttachment({ ...current, controlConsumed: true });
        this.deliver(
          host,
          { s: frame.header.s, k: "peer-command", from: state.connId },
          new TextEncoder().encode(JSON.stringify(admission))
        );
        return;
      }
      if (method === "QueueCommand" && !hostGrant) {
        this.rejectRequest(ws, frame, "peer_command_authority_required");
        return;
      }
      if (method === "ReadSessionCommand" && (state.userId !== hostState?.userId || state.projectScope !== hostState?.projectScope)) {
        this.rejectRequest(ws, frame, "session_scope_denied");
        return;
      }
      if (hostGrant && !rpcAllowedForScopedHost(frame.header, frame.payload, hostGrant)) {
        this.rejectRequest(ws, frame, "session_scope_denied");
        return;
      }
      this.deliver(host, { s: frame.header.s, k: frame.header.k, from: state.connId }, frame.payload);
      return;
    }
    if (frame.header.k === NUDGE_ACK_KIND) {
      const current = ws.deserializeAttachment();
      if (this.liveHost() !== ws || current?.superseded) return;
      let receipt;
      try {
        receipt = JSON.parse(new TextDecoder().decode(frame.payload));
      } catch {
        return;
      }
      if (!receipt || typeof receipt.chatId !== "string" || !CHAT_ID_RE.test(receipt.chatId) || typeof receipt.nudgeId !== "string" || frame.header.s !== receipt.chatId || current?.grant && current.grant.scope.sessionId !== receipt.chatId) return;
      this.ctx.storage.sql.exec("DELETE FROM pending_nudges WHERE chat_id = ? AND nudge_id = ?", receipt.chatId, receipt.nudgeId);
      await this.ctx.storage.sync();
      return;
    }
    const to = frame.header.to;
    if (!to) return;
    const target = this.liveClient(to);
    if (!target) {
      this.deliver(ws, { s: frame.header.s, k: RELAY_KIND, to }, encodeRelayError("client_gone"));
      return;
    }
    this.deliver(target, { s: frame.header.s, k: frame.header.k }, frame.payload);
  }
  async webSocketClose(ws) {
    const state = ws.deserializeAttachment();
    if (!state || state.durableSync !== true) return;
    if (state.superseded) return;
    if (state.role === "client") {
      const host = this.liveHost();
      if (host && await this.authorizeHost(host)) {
        this.deliver(host, { s: "", k: RELAY_KIND, from: state.connId }, encodeRelayError("client_closed"));
      }
      return;
    }
    if (this.liveHost(ws)) return;
    for (const client of this.ctx.getWebSockets()) {
      const cs = client.deserializeAttachment();
      if (cs?.role !== "client") continue;
      this.deliver(client, { s: "", k: RELAY_KIND }, encodeRelayError("host_closed"));
    }
  }
  async webSocketError(ws) {
    await this.webSocketClose(ws);
  }
  rejectRequest(ws, frame, code) {
    if (frame.header.k === "rpc") {
      try {
        const value = JSON.parse(new TextDecoder().decode(frame.payload));
        const id = value?.id;
        if (typeof id === "string" || Number.isSafeInteger(id) && id >= 0) {
          this.deliver(
            ws,
            { s: frame.header.s, k: "rpc" },
            new TextEncoder().encode(JSON.stringify({ id, err: code }))
          );
          return;
        }
      } catch {
      }
    }
    this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError(code));
  }
  deliver(ws, header, payload) {
    try {
      ws.send(encodeDeviceFrame(header, payload));
      return true;
    } catch {
      return false;
    }
  }
};
var pickLiveHost = /* @__PURE__ */ __name((hosts, now, exclude) => {
  let best;
  for (const host of hosts) {
    if (host.ws === exclude) continue;
    if (!best || host.lastSeenAt > best.lastSeenAt) best = host;
  }
  return best && now - best.lastSeenAt <= HOST_LIVENESS_MS ? best.ws : void 0;
}, "pickLiveHost");
var controlActorForRpc = /* @__PURE__ */ __name((payload) => {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    return value.method === "QueueCommand" && value.params?.command?.kind === "control" ? value.params.command.actorSubject : void 0;
  } catch {
    return void 0;
  }
}, "controlActorForRpc");
var rpcAllowedForScopedHost = /* @__PURE__ */ __name((header, payload, grant) => {
  if (header.k === "term") return grant.capabilities.includes("session.environment");
  if (header.k !== "rpc") return false;
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    if (value.method === "LocalDevice" && (!value.params || Object.keys(value.params).length === 0)) {
      return true;
    }
    if (value.method === "UploadChunk" || value.method === "UploadCommit") {
      return grant.capabilities.includes("session.files") && value.params?.sessionId === grant.scope.sessionId && (value.params.targetDeviceId === void 0 || value.params.targetDeviceId === grant.targetDeviceId);
    }
    if (value.method === "ReadSessionCommand") {
      const projection = value.params?.roomProjection;
      return grant.capabilities.includes(SESSION_READ) && value.params?.chatId === grant.scope.sessionId && typeof value.params.commandId === "string" && value.params.commandId.trim().length > 0 && (value.params.targetDeviceId === void 0 || value.params.targetDeviceId === grant.targetDeviceId) && (projection === void 0 || projection !== null && projection.projectId === grant.scope.projectId && projection.deploymentId === grant.scope.deploymentId && projection.sessionId === grant.scope.sessionId);
    }
    if (value.method === "DeliverPeerMessage") {
      return grant.capabilities.includes("session.chat") && value.params?.chatId === grant.scope.sessionId && value.params.command?.kind === "peerMessage";
    }
    return value.method === "QueueCommand" && value.params?.command?.sessionId === grant.scope.sessionId;
  } catch {
    return false;
  }
}, "rpcAllowedForScopedHost");
var rpcAllowedForDirectPeerReply = /* @__PURE__ */ __name((payload, targetSessionId) => {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    if (value.method === "LocalDevice") {
      return !value.params || Object.keys(value.params).length === 0;
    }
    const command = value.params?.command;
    return value.method === "DeliverPeerMessage" && value.params?.chatId === targetSessionId && typeof value.params.commandId === "string" && value.params.commandId.length > 0 && command?.kind === "peerMessage" && typeof command.sourceChatId === "string" && command.sourceChatId.length > 0 && typeof command.sourceDeviceId === "string" && command.sourceDeviceId.length > 0 && typeof command.threadId === "string" && command.threadId.length > 0 && command.threadId.length <= 256 && typeof command.replyTo === "string" && command.replyTo.length > 0 && command.replyTo.length <= 256 && Number.isSafeInteger(command.hopCount) && command.hopCount > 0 && command.hopCount <= 8 && typeof command.text === "string" && command.text.trim().length > 0 && command.text.length <= 256 * 1024;
  } catch {
    return false;
  }
}, "rpcAllowedForDirectPeerReply");
var rpcAllowedForPeerSession = /* @__PURE__ */ __name((payload, sourceSessionId, sourceDeploymentId, sourceDeviceId, targetSessionId) => {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    if (value.method === "LocalDevice") {
      return !value.params || Object.keys(value.params).length === 0;
    }
    return value.method === "DeliverPeerMessage" && value.params?.chatId === targetSessionId && typeof value.params.commandId === "string" && value.params.commandId.length > 0 && value.params.command?.kind === "peerMessage" && value.params.command.sourceChatId === sourceSessionId && value.params.command.sourceDeploymentId === sourceDeploymentId && value.params.command.sourceDeviceId === sourceDeviceId && typeof value.params.command.threadId === "string" && value.params.command.threadId.length > 0 && value.params.command.threadId.length <= 256 && (value.params.command.replyTo === null || typeof value.params.command.replyTo === "string" && value.params.command.replyTo.length > 0 && value.params.command.replyTo.length <= 256) && Number.isSafeInteger(value.params.command.hopCount) && value.params.command.hopCount >= 0 && value.params.command.hopCount <= 8 && typeof value.params.command.text === "string" && value.params.command.text.trim().length > 0 && value.params.command.text.length <= 256 * 1024;
  } catch {
    return false;
  }
}, "rpcAllowedForPeerSession");
var peerCommandAdmission = /* @__PURE__ */ __name((client, host, payload, now) => {
  if (!host || host.hostAuthorization !== "local" || host.grant || client.grant || host.userId !== client.userId || host.projectScope !== client.projectScope || !host.targetDeviceId || host.targetDeviceId !== client.targetDeviceId || !client.controlSessionId || client.controlConsumed || !client.joinedAt || client.joinedAt > now || now >= client.joinedAt + 3e4) return void 0;
  try {
    const request = JSON.parse(new TextDecoder().decode(payload));
    if (!["AdmitPeerCommand", "ReadSessionCommand"].includes(request.method) || !Number.isSafeInteger(request.id) || request.id < 0 || request.cancel || request.params?.chatId !== client.controlSessionId || typeof request.params.commandId !== "string" || !request.params.commandId.trim()) return void 0;
    if (request.params.deploymentId != null || request.params.controlDeploymentId != null) return void 0;
    if (request.params.roomProjection != null || request.params.targetDeviceId != null && request.params.targetDeviceId !== host.targetDeviceId) return void 0;
    const command = request.params.command;
    if (request.method === "AdmitPeerCommand") {
      if (!client.capabilities.includes("session.control")) return void 0;
      if (command?.kind === "control") {
        if (command.source !== "local" || command.ownerDeviceId !== host.targetDeviceId || command.actorSubject !== client.userId || typeof command.sessionId !== "string" || !command.sessionId.trim()) return void 0;
      } else if (!["run", "steer", "queue", "interrupt", "respondInput"].includes(command?.kind)) return void 0;
    }
    if (!client.capabilities.includes(requiredCapabilityForRpc({ s: "rpc", k: "rpc" }, payload))) return void 0;
    return { request, authority: {
      subject: client.userId,
      projectId: client.projectScope,
      deviceId: host.targetDeviceId,
      chatId: client.controlSessionId,
      expiresAt: client.joinedAt + 3e4
    } };
  } catch {
    return void 0;
  }
}, "peerCommandAdmission");
var requiredCapabilityForRpc = /* @__PURE__ */ __name((header, payload) => {
  if (header.k !== "rpc") return header.k === "term" ? "session.environment" : SESSION_READ;
  let value;
  try {
    value = JSON.parse(new TextDecoder().decode(payload));
  } catch {
    return "session.control";
  }
  if (value.method === "DeliverPeerMessage") return "session.chat";
  if (value.method === "LocalDevice") return SESSION_READ;
  if (value.method === "ReadSessionCommand") return SESSION_READ;
  if (value.method === "QueueCommand" || value.method === "AdmitPeerCommand") {
    const command = value.params?.command;
    if (command?.kind !== "control") return "session.control";
    const action = command.action?.action;
    if (action === "start" || action === "steer") return "session.chat";
    if (action === "environmentLifecycle") return "session.environment";
    if (action === "annotationCreate" || action === "annotationEdit" || action === "annotationResolve") {
      return "session.annotate";
    }
    return "session.control";
  }
  if (value.method?.includes("Attachment") || value.method?.startsWith("Upload")) {
    return "session.files";
  }
  if (value.method?.includes("Invite") || value.method?.includes("Grant")) {
    return "session.invite";
  }
  if (value.method?.includes("Terminal")) return "session.environment";
  if (value.method?.startsWith("Watch") || value.method?.startsWith("List") || value.method?.startsWith("Get") || value.method?.startsWith("Search") || value.method === "SyncStatus") {
    return SESSION_READ;
  }
  return "session.control";
}, "requiredCapabilityForRpc");
var encodeRelayError = /* @__PURE__ */ __name((code) => new TextEncoder().encode(JSON.stringify({ error: code })), "encodeRelayError");
var json2 = /* @__PURE__ */ __name((value, status = 200) => new Response(JSON.stringify(value), {
  status,
  headers: { "content-type": "application/json" }
}), "json");

// src/notifications.ts
var UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
var MAX_BODY_BYTES = 2048;
var MAX_USER_DEVICES = 16;
var MAX_PROJECT_DEVICES = 1024;
var REGISTRATION_TTL_MS = 30 * 24 * 60 * 6e4;
var MAX_CLOCK_SKEW_MS = 5e3;
var APNS_DIAGNOSTIC_REASONS = {
  BadDeviceToken: true,
  DeviceTokenNotForTopic: true,
  Unregistered: true,
  TopicDisallowed: true,
  InvalidProviderToken: true,
  ExpiredProviderToken: true,
  MissingProviderToken: true
};
var COPY = {
  input: "Needs your input.",
  error: "Encountered an error.",
  completion: "Finished working."
};
var notificationTitle = /* @__PURE__ */ __name((value) => {
  if (typeof value !== "string") return void 0;
  const normalized = value.replace(/\s+/gu, " ").trim();
  if (!normalized) return void 0;
  let title = "";
  let characters = 0;
  for (const character of normalized) {
    if (characters === 119) {
      return title + (normalized.length > title.length + character.length ? "\u2026" : character);
    }
    title += character;
    characters += 1;
  }
  return title;
}, "notificationTitle");
var stringField = /* @__PURE__ */ __name((row, key) => {
  const value = row instanceof LoroMap ? row.get(key) : row && typeof row === "object" && !isContainer(row) ? row[key] : void 0;
  if (isContainer(value)) {
    value.free();
    return void 0;
  }
  return typeof value === "string" ? value : void 0;
}, "stringField");
var sessionRefTitle = /* @__PURE__ */ __name((refs, chatId, userId) => {
  const key = `${new TextEncoder().encode(userId).byteLength}:${userId}:${chatId}`;
  const row = refs.get(key);
  let environment;
  try {
    if (stringField(row, "userId") !== userId || stringField(row, "chatId") !== chatId) return void 0;
    environment = row instanceof LoroMap ? row.get("environment") : row && typeof row === "object" && !isContainer(row) ? row.environment : void 0;
    return notificationTitle(stringField(environment, "name"));
  } finally {
    if (isContainer(environment)) environment.free();
    if (isContainer(row)) row.free();
  }
}, "sessionRefTitle");
var attentionTransition = /* @__PURE__ */ __name((previous, next, archived, now) => {
  if (!previous || archived || next.status === previous.status || next.updatedAt <= previous.updatedAt || next.updatedAt > now + MAX_CLOCK_SKEW_MS || now - next.updatedAt > 45e3) return void 0;
  if (next.status === "awaitingInput") return "input";
  if (next.status === "errored") return "error";
  if (previous.status === "working" && next.status === "idle") return "completion";
  return void 0;
}, "attentionTransition");
var parseRegistration = /* @__PURE__ */ __name((body, method) => {
  if (!body || typeof body !== "object" || Array.isArray(body)) return void 0;
  const value = body;
  if (typeof value.installationId !== "string" || !UUID.test(value.installationId)) return void 0;
  const installationId = value.installationId.toLowerCase();
  if (method === "DELETE") return { installationId };
  if (method !== "PUT" || typeof value.token !== "string" || !/^(?:[0-9a-f]{2}){16,100}$/.test(value.token) || value.environment !== "sandbox" && value.environment !== "production") return void 0;
  return { installationId, token: value.token, environment: value.environment };
}, "parseRegistration");
var json3 = /* @__PURE__ */ __name((value, status = 200) => Response.json(value, { status }), "json");
var base64url = /* @__PURE__ */ __name((bytes) => btoa(String.fromCharCode(...bytes)).replace(/=/g, "").replace(/\+/g, "-").replace(/\//g, "_"), "base64url");
var encodeJson = /* @__PURE__ */ __name((value) => base64url(new TextEncoder().encode(JSON.stringify(value))), "encodeJson");
var ApnsProvider = class {
  constructor(env, transport = fetch) {
    this.env = env;
    this.transport = transport;
  }
  env;
  transport;
  static {
    __name(this, "ApnsProvider");
  }
  cached;
  async authorization(now = Date.now()) {
    if (this.cached && now < this.cached.expiresAt) return this.cached.token;
    const { APNS_KEY_ID: keyId, APNS_TEAM_ID: teamId, APNS_PRIVATE_KEY: pem, APNS_TOPIC: topic } = this.env;
    if (!keyId || !teamId || !pem || !topic) throw new Error("apns_not_configured");
    if (!/^[A-Z0-9]{10}$/.test(keyId) || !/^[A-Z0-9]{10}$/.test(teamId) || !/^[A-Za-z0-9.-]{1,255}$/.test(topic)) throw new Error("apns_invalid_configuration");
    try {
      const match = pem.match(/^\s*-----BEGIN PRIVATE KEY-----([\s\S]+?)-----END PRIVATE KEY-----\s*$/);
      if (!match) throw new Error("invalid key");
      const der = Uint8Array.from(atob(match[1].replace(/\s/g, "")), (char) => char.charCodeAt(0));
      const key = await crypto.subtle.importKey("pkcs8", der, { name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]);
      const content = `${encodeJson({ alg: "ES256", kid: keyId })}.${encodeJson({ iss: teamId, iat: Math.floor(now / 1e3) })}`;
      const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key, new TextEncoder().encode(content));
      const token = `${content}.${base64url(new Uint8Array(signature))}`;
      this.cached = { token, expiresAt: now + 50 * 6e4 };
      return token;
    } catch {
      throw new Error("apns_invalid_configuration");
    }
  }
  async send(device, chatId, projectScope, userId, attention, title, authorization) {
    const jwt = authorization ?? await this.authorization();
    const host = device.environment === "sandbox" ? "api.sandbox.push.apple.com" : "api.push.apple.com";
    const transport = this.transport;
    const response = await transport(`https://${host}/3/device/${device.token}`, {
      method: "POST",
      headers: {
        authorization: `bearer ${jwt}`,
        "apns-topic": this.env.APNS_TOPIC,
        "apns-push-type": "alert",
        "apns-priority": "10",
        "apns-expiration": String(Math.floor(Date.now() / 1e3) + 45),
        "apns-collapse-id": chatId,
        "content-type": "application/json"
      },
      body: JSON.stringify({
        aps: { alert: { title: notificationTitle(title) ?? `Session ${chatId.slice(0, 8)}`, body: COPY[attention] }, sound: "default" },
        chatId,
        projectScope,
        userId
      }),
      signal: AbortSignal.timeout(1e4)
    });
    const status = response.status;
    const apnsId = response.headers.get("apns-id") ?? void 0;
    if (this.env.APNS_TOPIC === "ai.ashler.crew.staging") console.info("Crew push diagnostic", JSON.stringify({ stage: "apns_response", at: Date.now(), status, apnsId }));
    if (response.ok) return { status, apnsId, remove: false };
    const body = await response.json().catch(() => ({}));
    const reason = typeof body.reason === "string" && APNS_DIAGNOSTIC_REASONS[body.reason] === true ? body.reason : "Other";
    console.warn("Crew push delivery rejected", response.status, reason);
    if (response.status === 410 && body.reason === "Unregistered") {
      return { status, apnsId, remove: true, invalidatedAt: typeof body.timestamp === "number" ? body.timestamp : void 0 };
    }
    if (response.status === 400 && (body.reason === "BadDeviceToken" || body.reason === "DeviceTokenNotForTopic")) {
      return { status, apnsId, remove: true };
    }
    if (body.reason === "ExpiredProviderToken") this.cached = void 0;
    return { status, apnsId, remove: false };
  }
};
var WorkspaceNotifications = class {
  constructor(storage, env) {
    this.storage = storage;
    this.env = env;
    this.provider = new ApnsProvider(env);
    storage.transactionSync(() => {
      const columns = [...storage.sql.exec("PRAGMA table_info(notification_devices)")];
      if (columns.some((column) => column.name === "bearer")) {
        storage.sql.exec("DROP TABLE notification_devices");
      }
      storage.sql.exec("CREATE TABLE IF NOT EXISTS notification_devices (installationId TEXT PRIMARY KEY, userId TEXT NOT NULL, token TEXT NOT NULL, environment TEXT NOT NULL, revision TEXT NOT NULL, registeredAt INTEGER NOT NULL, sealedCredential TEXT NOT NULL)");
      storage.sql.exec("CREATE UNIQUE INDEX IF NOT EXISTS notification_token ON notification_devices (token, environment)");
      storage.sql.exec("CREATE INDEX IF NOT EXISTS notification_registration_age ON notification_devices (registeredAt)");
    });
    storage.sql.exec("CREATE TABLE IF NOT EXISTS notification_status (chatId TEXT PRIMARY KEY, status TEXT NOT NULL, updatedAt INTEGER NOT NULL, active INTEGER NOT NULL, edgeAt INTEGER NOT NULL)");
  }
  storage;
  env;
  static {
    __name(this, "WorkspaceNotifications");
  }
  provider;
  delivery = Promise.resolve();
  credentialKey;
  diagnosticQueuedBatches = 0;
  trace(stage, timing = {}) {
    if (this.env.ENVIRONMENT === "staging") console.info("Crew push diagnostic", JSON.stringify({ stage, at: Date.now(), ...timing }));
  }
  encryptionKey() {
    if (!this.credentialKey) {
      const encoded = this.env.NOTIFICATION_CREDENTIAL_KEY;
      if (!encoded || !/^[A-Za-z0-9+/]{43}=$/.test(encoded)) throw new Error("notification_credential_invalid_configuration");
      const bytes = Uint8Array.from(atob(encoded), (char) => char.charCodeAt(0));
      if (bytes.length !== 32 || btoa(String.fromCharCode(...bytes)) !== encoded) throw new Error("notification_credential_invalid_configuration");
      this.credentialKey = crypto.subtle.importKey("raw", bytes, "AES-GCM", false, ["encrypt", "decrypt"]);
    }
    return this.credentialKey;
  }
  credentialContext(installationId, userId) {
    const scope = this.env.SCAFFOLD_PROJECT_SCOPE?.trim();
    if (!scope) throw new Error("notification_credential_invalid_configuration");
    return new TextEncoder().encode(JSON.stringify(["crew.notification-credential.v1", installationId, userId, scope]));
  }
  async sealCredential(installationId, userId, bearer) {
    const additionalData = this.credentialContext(installationId, userId);
    const key = await this.encryptionKey();
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData }, key, new TextEncoder().encode(bearer));
    return `v1.${base64url(iv)}.${base64url(new Uint8Array(ciphertext))}`;
  }
  async openCredential(device) {
    const parts = device.sealedCredential.split(".");
    if (parts.length !== 3 || parts[0] !== "v1" || !/^[A-Za-z0-9_-]{16}$/.test(parts[1]) || !/^[A-Za-z0-9_-]+$/.test(parts[2])) throw new Error("notification_credential_invalid");
    const decode2 = /* @__PURE__ */ __name((value) => Uint8Array.from(atob(value.replace(/-/g, "+").replace(/_/g, "/")), (char) => char.charCodeAt(0)), "decode");
    const additionalData = this.credentialContext(device.installationId, device.userId);
    const plaintext = await crypto.subtle.decrypt({ name: "AES-GCM", iv: decode2(parts[1]), additionalData }, await this.encryptionKey(), decode2(parts[2]));
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(plaintext);
  }
  async register(request, userId, bearer) {
    if (request.method !== "PUT" && request.method !== "DELETE") return json3({ error: "method_not_allowed" }, 405);
    if (userId.length > 512 || bearer.length > 8192) return json3({ error: "too_large" }, 413);
    const reader = request.body?.getReader();
    if (!reader) return json3({ error: "invalid_request" }, 400);
    const bytes = new Uint8Array(MAX_BODY_BYTES);
    let size = 0;
    try {
      while (true) {
        const { value: value2, done } = await reader.read();
        if (done) break;
        if (size + value2.byteLength > MAX_BODY_BYTES) {
          await reader.cancel();
          return json3({ error: "too_large" }, 413);
        }
        bytes.set(value2, size);
        size += value2.byteLength;
      }
    } catch {
      return json3({ error: "invalid_request" }, 400);
    }
    let value;
    try {
      value = parseRegistration(JSON.parse(new TextDecoder().decode(bytes.subarray(0, size))), request.method);
    } catch {
      return json3({ error: "invalid_request" }, 400);
    }
    if (!value) return json3({ error: "invalid_request" }, 400);
    let sealedCredential;
    if (request.method === "PUT") {
      try {
        sealedCredential = await this.sealCredential(value.installationId, userId, bearer);
      } catch {
        return json3({ error: "notification_credential_invalid_configuration" }, 503);
      }
      try {
        await this.provider.authorization();
      } catch (error) {
        return json3({ error: error instanceof Error ? error.message : "apns_invalid_configuration" }, 503);
      }
    }
    return this.storage.transactionSync(() => {
      this.storage.sql.exec("DELETE FROM notification_devices WHERE registeredAt < ?", Date.now() - REGISTRATION_TTL_MS);
      const existing = [...this.storage.sql.exec("SELECT * FROM notification_devices WHERE installationId = ?", value.installationId)][0];
      if (existing && existing.userId !== userId) return json3({ error: "forbidden" }, 403);
      if (request.method === "DELETE") {
        this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ? AND userId = ?", value.installationId, userId);
        return json3({ ok: true });
      }
      const tokenOwner = [...this.storage.sql.exec("SELECT * FROM notification_devices WHERE token = ? AND environment = ?", value.token, value.environment)][0];
      if (tokenOwner && tokenOwner.userId !== userId) return json3({ error: "forbidden" }, 403);
      if (!existing && !tokenOwner) {
        const counts = [...this.storage.sql.exec("SELECT COUNT(*) AS total, COALESCE(SUM(userId = ?), 0) AS owned FROM notification_devices", userId)][0];
        if (counts.total >= MAX_PROJECT_DEVICES || counts.owned >= MAX_USER_DEVICES) return json3({ error: "subscription_limit" }, 429);
      }
      if (tokenOwner && tokenOwner.installationId !== value.installationId) {
        this.storage.sql.exec("DELETE FROM notification_devices WHERE installationId = ?", tokenOwner.installationId);
      }
      this.storage.sql.exec("INSERT INTO notification_devices (installationId, userId, token, environment, revision, registeredAt, sealedCredential) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(installationId) DO UPDATE SET token = excluded.token, environment = excluded.environment, revision = excluded.revision, registeredAt = excluded.registeredAt, sealedCredential = excluded.sealedCredential", value.installationId, userId, value.token, value.environment, crypto.randomUUID(), Date.now(), sealedCredential);
      return json3({ ok: true });
    });
  }
  /** Called before import to baseline persisted replay, then after accepted
   * import. Durable monotonic rows prevent heartbeat/replay/hibernation alerts. */
  observe(doc, baseline, now = Date.now()) {
    let phase = "registration_lookup";
    let sessionsMap;
    let chatsMap;
    let refsMap;
    let recipients;
    try {
      if (![...this.storage.sql.exec("SELECT 1 FROM notification_devices WHERE registeredAt >= ? LIMIT 1", now - REGISTRATION_TTL_MS)].length) {
        this.trace("observe_skip", { reason: "no_eligible_registration", baseline, observedAt: now });
        return [];
      }
      phase = "sessions_get_map";
      sessionsMap = doc.getMap("sessions");
      phase = "chats_get_map";
      chatsMap = doc.getMap("chats");
      const events = [];
      phase = "status_transaction";
      this.storage.transactionSync(() => {
        phase = "status_deactivate";
        this.storage.sql.exec("UPDATE notification_status SET active = 0 WHERE active = 1");
        phase = "session_keys";
        for (const chatId of sessionsMap.keys()) {
          if (typeof chatId !== "string" || !UUID.test(chatId)) continue;
          const diagnose = chatId === "d8d19839-82fd-44b5-9c5a-7c9ac0106c14";
          phase = "session_get";
          const raw = sessionsMap.get(chatId);
          let chat;
          let status;
          let updatedAt;
          let archived;
          try {
            if (!raw || typeof raw !== "object" || isContainer(raw) && !(raw instanceof LoroMap)) continue;
            phase = "session_fields";
            status = raw instanceof LoroMap ? raw.get("status") : raw.status;
            updatedAt = raw instanceof LoroMap ? raw.get("updatedAt") : raw.updatedAt;
            if (typeof status !== "string" || typeof updatedAt !== "number" || !["idle", "working", "awaitingInput", "errored"].includes(status) || !Number.isSafeInteger(updatedAt) || updatedAt > now + MAX_CLOCK_SKEW_MS || updatedAt < 0) {
              if (diagnose && typeof updatedAt === "number" && Number.isSafeInteger(updatedAt) && updatedAt > now + MAX_CLOCK_SKEW_MS) this.trace("observe_skip", { reason: "future", baseline, observedAt: now, updatedAt });
              continue;
            }
            const next = { status, updatedAt };
            phase = "chat_get";
            chat = chatsMap.get(chatId);
            archived = chat instanceof LoroMap ? chat.get("archived") : chat && typeof chat === "object" && !isContainer(chat) ? chat.archived : void 0;
            const active = archived === false;
            phase = "status_lookup";
            const previous = [...this.storage.sql.exec("SELECT status, updatedAt FROM notification_status WHERE chatId = ?", chatId)][0];
            if (previous && next.updatedAt <= previous.updatedAt) {
              if (diagnose) this.trace("observe_skip", { reason: "not_newer", baseline, observedAt: now, previousUpdatedAt: previous.updatedAt, updatedAt: next.updatedAt });
              phase = "status_reactivate";
              this.storage.sql.exec("UPDATE notification_status SET active = ? WHERE chatId = ?", active ? 1 : 0, chatId);
              continue;
            }
            phase = "attention_decision";
            const attention = baseline ? void 0 : attentionTransition(previous, next, !active, now);
            if (diagnose) this.trace("observe", {
              baseline,
              observedAt: now,
              updatedAt: next.updatedAt,
              ageMs: now - next.updatedAt,
              previous: previous?.status ?? "none",
              next: next.status,
              active,
              decision: baseline ? "baseline" : !previous ? "initial" : !active ? "inactive" : now - next.updatedAt > 45e3 ? "stale" : attention ?? "non_attention"
            });
            phase = "status_upsert";
            this.storage.sql.exec("INSERT INTO notification_status (chatId, status, updatedAt, active, edgeAt) VALUES (?, ?, ?, ?, ?) ON CONFLICT(chatId) DO UPDATE SET edgeAt = CASE WHEN notification_status.status != excluded.status THEN excluded.edgeAt ELSE notification_status.edgeAt END, status = excluded.status, updatedAt = excluded.updatedAt, active = excluded.active", chatId, next.status, next.updatedAt, active ? 1 : 0, next.updatedAt);
            if (attention) {
              const title = notificationTitle(stringField(chat, "title")) ?? `Session ${chatId.slice(0, 8)}`;
              refsMap ??= doc.getMap("sessionRefs");
              recipients ??= [...this.storage.sql.exec("SELECT DISTINCT userId FROM notification_devices WHERE registeredAt >= ?", now - REGISTRATION_TTL_MS)];
              let titlesByUser;
              for (const { userId } of recipients) {
                const name = sessionRefTitle(refsMap, chatId, userId);
                if (name) (titlesByUser ??= /* @__PURE__ */ new Map()).set(userId, name);
              }
              events.push({ chatId, attention, updatedAt: next.updatedAt, title, titlesByUser });
            }
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
      this.trace("observe_exception", { phase, error: (error instanceof Error ? error.message : String(error)).slice(0, 256), stack: error instanceof Error ? error.stack?.split("\n").slice(0, 9).join("\n") : void 0 });
      throw new Error(`${phase}: ${(error instanceof Error ? error.message : String(error)).slice(0, 256)}`, { cause: error });
    } finally {
      try {
        sessionsMap?.free();
      } catch (error) {
        this.trace("observe_cleanup_exception", { phase: "sessions_free", error: (error instanceof Error ? error.message : String(error)).slice(0, 256) });
      }
      try {
        chatsMap?.free();
      } catch (error) {
        this.trace("observe_cleanup_exception", { phase: "chats_free", error: (error instanceof Error ? error.message : String(error)).slice(0, 256) });
      }
      try {
        refsMap?.free();
      } catch {
        this.trace("observe_cleanup_exception", { phase: "refs_free" });
      }
    }
  }
  deliver(events, projectScope) {
    const queuedAt = Date.now();
    const queuedAhead = this.diagnosticQueuedBatches++;
    this.trace("queue_enqueued", { queuedAt, queuedAhead, events: events.length });
    const delivery = this.delivery.then(() => {
      this.trace("queue_started", { queuedAt, queuedAhead, waitMs: Date.now() - queuedAt });
      return this.deliverInOrder(events, projectScope);
    });
    this.delivery = delivery.catch(() => {
      console.warn("Crew push delivery failed");
    }).finally(() => {
      this.diagnosticQueuedBatches--;
    });
    return delivery;
  }
  async deliverInOrder(events, projectScope) {
    const syncStartedAt = Date.now();
    this.trace("storage_sync_started");
    await this.storage.sync();
    this.trace("storage_sync_finished", { elapsedMs: Date.now() - syncStartedAt });
    this.storage.sql.exec("DELETE FROM notification_devices WHERE registeredAt < ?", Date.now() - REGISTRATION_TTL_MS);
    for (const event of events) {
      if (Date.now() - event.updatedAt > 45e3) {
        this.trace("delivery_skipped", { reason: "stale_at_queue_head", updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
        continue;
      }
      const devices = [...this.storage.sql.exec("SELECT * FROM notification_devices")];
      this.trace("delivery_started", { updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt, recipients: devices.length });
      for (let offset = 0; offset < devices.length; offset += 8) {
        await Promise.all(devices.slice(offset, offset + 8).map(async (device) => {
          try {
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
            if (identity.userId !== device.userId || identity.projectScope !== projectScope || !identity.capabilities.includes("session.read")) {
              this.trace("delivery_skipped", { reason: "identity_mismatch", updatedAt: event.updatedAt });
              return;
            }
            const authorization = await this.provider.authorization();
            const current = [...this.storage.sql.exec("SELECT * FROM notification_devices WHERE installationId = ?", device.installationId)][0];
            const latest = [...this.storage.sql.exec("SELECT status, updatedAt, active, edgeAt FROM notification_status WHERE chatId = ?", event.chatId)][0];
            if (!current || current.revision !== device.revision || !latest?.active || latest.edgeAt !== event.updatedAt || Date.now() - event.updatedAt > 45e3) {
              this.trace("delivery_skipped", { reason: !current ? "unregistered" : current.revision !== device.revision ? "registration_changed" : !latest?.active ? "inactive" : latest.edgeAt !== event.updatedAt ? "superseded" : "stale_before_send", updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
              return;
            }
            this.trace("apns_started", { updatedAt: event.updatedAt, ageMs: Date.now() - event.updatedAt });
            const delivery = await this.provider.send(device, event.chatId, projectScope, device.userId, event.attention, event.titlesByUser?.get(device.userId) ?? event.title, authorization);
            this.trace("apns_finished", { updatedAt: event.updatedAt, status: delivery.status, apnsId: delivery.apnsId, removeRegistration: delivery.remove });
            if (delivery.remove && (delivery.invalidatedAt === void 0 || device.registeredAt <= delivery.invalidatedAt)) {
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
};

// src/session-room.ts
var AUTH_PROJECT_HEADER2 = "x-comet-auth-project";
var AUTH_CAPABILITIES_HEADER2 = "x-comet-auth-capabilities";
var SESSION_READ2 = "session.read";
var GRANT_ID_RE2 = /^[A-Za-z0-9_-]{1,128}$/;
var PUBLISH_CAPABILITIES = {
  "session.chat": true,
  "session.control": true,
  "session.annotate": true,
  "session.files": true
};
var parseCapabilities2 = /* @__PURE__ */ __name((request) => (request.headers.get(AUTH_CAPABILITIES_HEADER2) ?? "").split(/\s+/).filter(Boolean), "parseCapabilities");
var canPublish = /* @__PURE__ */ __name((capabilities) => capabilities.some((capability) => PUBLISH_CAPABILITIES[capability] === true), "canPublish");
var SESSION_OWNER_AUTH_VALUE = "verify";
var sessionOwnerForConnection = /* @__PURE__ */ __name((currentOwnerUserId, userId, workspace, device) => currentOwnerUserId ?? (!workspace && !device ? userId : void 0), "sessionOwnerForConnection");
var SESSION_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
var canonicalSessionId = /* @__PURE__ */ __name((value) => {
  if (!value || !SESSION_ID_RE.test(value)) return void 0;
  return value.toLowerCase();
}, "canonicalSessionId");
var DAY_MS = 24 * 60 * 60 * 1e3;
var RETAIN_MS = RETAIN_DAYS * DAY_MS;
var FRAGMENT_TTL_MS = 3e4;
var FRAGMENT_BYTES = 2e5;
var MAX_REASSEMBLED_BYTES = 64 * 1024 * 1024;
var MAX_FRAGMENT_COUNT = 1024;
var MAX_PRESENCE_UPDATE_BYTES = 16 * 1024;
var WORKSPACE_PRESENCE_TTL_MS = 3e4;
var MAX_WORKSPACE_PRESENCE_PEERS = 128;
var MAX_RESET_SEED_BYTES = 8 * 1024 * 1024;
var readBoundedBody = /* @__PURE__ */ __name(async (request, maxBytes) => {
  const advertised = Number(request.headers.get("content-length"));
  if (Number.isFinite(advertised) && advertised > maxBytes) return null;
  const reader = request.body?.getReader();
  if (!reader) return new Uint8Array();
  const chunks = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      if (size + value.byteLength > maxBytes) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
      size += value.byteLength;
    }
  } finally {
    reader.releaseLock();
  }
  if (chunks.length === 1) return chunks[0];
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}, "readBoundedBody");
var MAX_CHECKPOINTS = 36;
var wasmPoisonStrikes = 0;
var WASM_POISON_ABORT_AFTER = 3;
var DOC_IDLE_RELEASE_MS = 6e4;
var TRIM_FORCE_BYTES = 512 * 1024;
var enforceDeviceGrantAuthority = /* @__PURE__ */ __name(async (ws, state, now, validate) => {
  const grantId = state?.grantId;
  const grantExpiresAt = state?.grantExpiresAt;
  const hasGrantState = grantId !== void 0 || grantExpiresAt !== void 0;
  let valid = state !== null;
  if (valid && hasGrantState) {
    if (typeof grantId !== "string" || !GRANT_ID_RE2.test(grantId) || typeof grantExpiresAt !== "number" || !Number.isSafeInteger(grantExpiresAt) || grantExpiresAt <= now) {
      valid = false;
    } else {
      try {
        valid = await validate(grantId) === true;
      } catch {
        valid = false;
      }
    }
  }
  if (valid) return true;
  try {
    ws.close(4403, "device grant invalid");
  } catch {
  }
  return false;
}, "enforceDeviceGrantAuthority");
var SessionRoom = class {
  static {
    __name(this, "SessionRoom");
  }
  ctx;
  env;
  blobs;
  notifications;
  /** Lazily materialized doc — the log is authoritative; this is a cache. */
  doc;
  docLoad;
  /** Legacy accepted deltas may still lack dependencies after cold replay. */
  docPendingEnds;
  eph;
  /** Bounded byte snapshots for workspace presence. Avoids another Loro WASM
   * allocation while letting fresh joiners observe peers before the next
   * 15-second heartbeat. Lost on hibernation by design. */
  workspacePresence = /* @__PURE__ */ new Map();
  /** In-memory fragment reassembly. Lost on hibernation → the sender gets a
   * FragmentTimeout ack for the unknown batch and resends — self-healing. */
  fragments = /* @__PURE__ */ new Map();
  fragmentTimer;
  applyingFragmentBytes = 0;
  applyingFragmentParts = 0;
  /** Revocations delivered while this instance is live close the TOCTOU gap
   * between an authority response and a handler's final mutation/send. */
  revokedGrants = /* @__PURE__ */ new Set();
  /** Idle-doc release bookkeeping (see DOC_IDLE_RELEASE_MS / touchDoc). */
  docIdleTimer;
  lastDocUse = 0;
  constructor(ctx, env) {
    this.ctx = ctx;
    this.env = env;
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }
  // ── meta helpers ──────────────────────────────────────────────────────────
  getMeta(key) {
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value;
  }
  setMeta(key, value) {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
  }
  // ── HTTP surface (only reachable through the authed Worker) ──────────────
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/grant-revoked" && request.method === "POST") {
      if (request.headers.get(GRANT_EVENT_HEADER) !== "revoke") {
        return new Response("forbidden", { status: 403 });
      }
      let body;
      try {
        body = await request.json();
      } catch {
        return new Response("invalid request", { status: 400 });
      }
      if (!body || typeof body !== "object" || !("grantId" in body) || typeof body.grantId !== "string" || !GRANT_ID_RE2.test(body.grantId)) {
        return new Response("invalid request", { status: 400 });
      }
      await this.revokeGrant(body.grantId);
      return new Response(null, { status: 204 });
    }
    if (url.pathname === "/authorize-owner") {
      if (request.headers.get(SESSION_OWNER_AUTH_HEADER) !== SESSION_OWNER_AUTH_VALUE) {
        return new Response("forbidden", { status: 403 });
      }
      if (request.method !== "GET") {
        return new Response("method not allowed", { status: 405 });
      }
      const candidateUserId = request.headers.get(AUTH_USER_HEADER);
      const candidateProjectScope = request.headers.get(AUTH_PROJECT_HEADER2);
      if (!candidateUserId || !candidateProjectScope) {
        return new Response("unauthenticated", { status: 401 });
      }
      const ownsSession = this.getMeta("projectScope") === candidateProjectScope && this.getMeta("ownerUserId") === candidateUserId;
      if (!ownsSession) return json4({ ownsSession: false });
      let deviceId = this.getMeta("hostDeviceOwnerUserId") === candidateUserId && this.getMeta("hostDeviceSyncProtocol") === DURABLE_SYNC_PROTOCOL ? this.getMeta("hostDeviceId") : void 0;
      if (!deviceId) {
        const liveDevices = new Set(
          this.ctx.getWebSockets().map((socket) => socket.deserializeAttachment()).filter((state) => state?.userId === candidateUserId && state.projectScope === candidateProjectScope && !state.grantId && !state.workspace && state.durableSync === true && canPublish(state.capabilities)).map((state) => state?.deviceId).filter((value) => typeof value === "string" && GRANT_ID_RE2.test(value))
        );
        if (liveDevices.size === 1) {
          deviceId = liveDevices.values().next().value;
          if (deviceId) {
            this.setMeta("hostDeviceId", deviceId);
            this.setMeta("hostDeviceOwnerUserId", candidateUserId);
            this.setMeta("hostDeviceSyncProtocol", DURABLE_SYNC_PROTOCOL);
          }
        }
      }
      return json4({ ownsSession: true, ...deviceId ? { deviceId } : {} });
    }
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const projectScope = request.headers.get(AUTH_PROJECT_HEADER2);
    const capabilities = parseCapabilities2(request);
    if (!projectScope || !capabilities.includes(SESSION_READ2)) {
      return new Response("forbidden", { status: 403 });
    }
    const boundScope = this.getMeta("projectScope");
    if (boundScope && boundScope !== projectScope) return new Response("forbidden", { status: 403 });
    const durableSync = request.headers.get(SYNC_PROTOCOL_HEADER) === DURABLE_SYNC_PROTOCOL;
    const writeCapability = request.method === "POST" ? url.pathname === "/append" ? "session.chat" : url.pathname === "/reset-log" ? "session.control" : url.pathname === "/diff" ? "session.files" : void 0 : void 0;
    if (writeCapability && !capabilities.includes(writeCapability)) return json4({ error: "forbidden" }, 403);
    if (writeCapability && !durableSync) return json4({ error: "crew_update_required", message: CREW_UPDATE_REQUIRED }, 426);
    if (!boundScope) this.setMeta("projectScope", projectScope);
    const workspace = request.headers.get(ROOM_KIND_HEADER) === "workspace";
    if (workspace || !this.getMeta("roomKind")) this.setMeta("roomKind", workspace ? "workspace" : "session");
    if (url.pathname === "/notifications/device") {
      if (!workspace || request.headers.has(AUTH_GRANT_HEADER)) return json4({ error: "forbidden" }, 403);
      const bearer = request.headers.get(NOTIFICATION_BEARER_HEADER);
      if (!bearer) return json4({ error: "forbidden" }, 403);
      return this.workspaceNotifications().register(request, userId, bearer);
    }
    if (url.pathname === "/ws") {
      const chatId = url.searchParams.get("chatId") ?? "";
      const encodedGrant = request.headers.get(AUTH_GRANT_HEADER);
      const grant = encodedGrant === null ? void 0 : parseTrustedDeviceGrant(encodedGrant, userId, projectScope, Date.now());
      if (encodedGrant !== null && (!grant || (workspace ? chatId !== `ws4/${projectScope}` : grant.scope.sessionId !== chatId))) {
        return new Response("forbidden", { status: 403 });
      }
      if (grant && !workspace) {
        const boundDeploymentId = this.getMeta("deploymentId");
        const boundSessionId = this.getMeta("scopedSessionId");
        if (boundDeploymentId && boundDeploymentId !== grant.scope.deploymentId || boundSessionId && boundSessionId !== grant.scope.sessionId) {
          return new Response("forbidden", { status: 403 });
        }
        if (!boundDeploymentId) this.setMeta("deploymentId", grant.scope.deploymentId);
        if (!boundSessionId) this.setMeta("scopedSessionId", grant.scope.sessionId);
      }
      const currentOwnerUserId = this.getMeta("ownerUserId");
      const ownerUserId = sessionOwnerForConnection(
        currentOwnerUserId,
        userId,
        workspace,
        grant !== void 0
      );
      if (!currentOwnerUserId && ownerUserId) this.setMeta("ownerUserId", ownerUserId);
      if (chatId && !this.getMeta("chatId")) this.setMeta("chatId", chatId);
      const deviceId = url.searchParams.get("device") ?? void 0;
      if (durableSync && !workspace && !grant && userId === ownerUserId && canPublish(capabilities) && deviceId && GRANT_ID_RE2.test(deviceId) && (!this.getMeta("hostDeviceId") || this.getMeta("hostDeviceOwnerUserId") !== ownerUserId || this.getMeta("hostDeviceSyncProtocol") !== DURABLE_SYNC_PROTOCOL)) {
        this.setMeta("hostDeviceId", deviceId);
        this.setMeta("hostDeviceOwnerUserId", userId);
        this.setMeta("hostDeviceSyncProtocol", DURABLE_SYNC_PROTOCOL);
      }
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state = {
        userId,
        projectScope,
        capabilities,
        durableSync,
        rooms: [],
        ...workspace ? { workspace } : {},
        ...deviceId ? { deviceId } : {},
        ...grant ? { grantId: grant.grantId, grantExpiresAt: grant.expiresAt } : {}
      };
      pair[1].serializeAttachment(state);
      await this.scheduleGrantExpiryAlarm();
      console.log(
        "socket accepted",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${deviceId ?? "unattributed"}`,
        `sockets=${this.ctx.getWebSockets().length}`
      );
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    const owner = this.getMeta("owner");
    if (url.pathname === "/stats" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      await this.flush();
      const updateRows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n;
      return json4({
        chatId: this.getMeta("chatId") ?? null,
        connectedSockets: this.ctx.getWebSockets().length,
        updateRows,
        updateLogBytes: Number(this.getMeta("updateBytes") ?? "0"),
        snapshotBytes: this.blobs.byteLength("snapshot") ?? 0,
        // Cold-start cost of the LAST materialization — the wedge-risk gauge
        // (2026-07-30: this creeping toward the CPU limit was invisible).
        lastReplayMs: Number(this.getMeta("lastReplayMs") ?? "0"),
        lastReplayRows: Number(this.getMeta("lastReplayRows") ?? "0"),
        lastReplayBatches: Number(this.getMeta("lastReplayBatches") ?? "0"),
        lastColdMs: Number(this.getMeta("lastColdMs") ?? "0"),
        // True between a wedge-break log drop and the first re-uploaded state
        // (the nightly backup is paused in that window).
        postReset: this.getMeta("postReset") === "1",
        tailCached: this.getMeta("tailDirty") !== "1" && this.blobs.byteLength("tail") !== void 0,
        diffPublished: this.blobs.byteLength("diff") !== void 0,
        checkpoints: JSON.parse(this.getMeta("checkpoints") ?? "[]").length,
        lastTrimAt: this.getMeta("lastTrimAt") ?? null,
        backupDirty: this.getMeta("backupDirty") === "1",
        // Non-zero while replay is in flight or failing; never an erase gate.
        replayAttempts: Number(this.getMeta("replayAttempts") ?? "0")
      });
    }
    if (url.pathname === "/tail" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      return json4(await this.currentTail());
    }
    if (url.pathname === "/diff" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      const diff = getJsonBlob(this.blobs, "diff");
      return diff === void 0 ? json4({ error: "not_found" }, 404) : json4(diff);
    }
    if (url.pathname === "/diff" && request.method === "POST") {
      if (!capabilities.includes("session.files")) {
        return json4({ error: "forbidden" }, 403);
      }
      if (!workspace) {
        if (!owner) this.setMeta("owner", projectScope);
        else if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      putJsonBlob(this.blobs, "diff", await request.json());
      return json4({ ok: true });
    }
    if (url.pathname === "/snapshot" && request.method === "GET") {
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      await this.flush();
      const doc = await this.ensureDoc();
      this.assertLoroMaterialized(doc, this.docPendingEnds);
      const bytes = doc.export({ mode: "snapshot" });
      return new Response(bytes, {
        headers: { "content-type": "application/octet-stream" }
      });
    }
    if (url.pathname === "/append" && request.method === "POST") {
      if (!capabilities.includes("session.chat")) {
        return json4({ error: "forbidden" }, 403);
      }
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      const body = await readBoundedBody(request, MAX_REASSEMBLED_BYTES);
      if (body === null) return json4({ error: "too_large" }, 413);
      let doc = await this.ensureDoc();
      if (workspace) this.notifyWorkspace(doc, projectScope, true);
      let repair;
      try {
        ({ doc, repair } = this.importLoroUpdates(doc, [body]));
      } catch (error) {
        this.escalateWasmPoisoning(error);
        return json4({ error: "invalid_update" }, 400);
      }
      if (workspace) this.notifyWorkspace(doc, projectScope);
      await this.ctx.storage.sync();
      const roomId = this.getMeta("chatId") ?? "";
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment();
        if (!state?.rooms.includes(CrdtType.Loro)) continue;
        if (!await this.authorizeSocket(ws, state)) continue;
        this.sendUpdates(ws, CrdtType.Loro, roomId, repair ? [body, repair] : [body]);
      }
      return json4({ ok: true });
    }
    if (url.pathname === "/reset-log" && request.method === "POST") {
      if (!capabilities.includes("session.control")) {
        return json4({ error: "forbidden" }, 403);
      }
      if (!workspace) {
        if (!owner) return json4({ error: "not_found" }, 404);
        if (owner !== projectScope) return json4({ error: "forbidden" }, 403);
      }
      const seed = await readBoundedBody(request, MAX_RESET_SEED_BYTES);
      if (seed === null) return json4({ error: "too_large" }, 413);
      if (seed.byteLength > 0) {
        if (!workspace) return json4({ error: "workspace_seed_required" }, 400);
        let validationStage = "decode";
        try {
          const metadata = decodeImportBlobMeta(seed, true);
          validationStage = "mode";
          try {
            if (metadata.mode !== "snapshot" && metadata.mode !== "shallow-snapshot" && metadata.mode !== "outdated-snapshot") {
              return json4({ error: "complete_snapshot_required" }, 400);
            }
          } finally {
            validationStage = "free-start";
            metadata.partialStartVersionVector.free();
            validationStage = "free-end";
            metadata.partialEndVersionVector.free();
          }
          validationStage = "construct";
          const candidate = new LoroDoc();
          try {
            validationStage = "import";
            const imported = candidate.import(seed);
            const pendingEnds = /* @__PURE__ */ new Map();
            for (const [peer, span] of imported.pending ?? []) pendingEnds.set(peer, span.end);
            validationStage = "materialize";
            this.assertLoroMaterialized(candidate, pendingEnds);
            validationStage = "decode-state";
            candidate.toJSON();
          } finally {
            candidate.free();
          }
        } catch (error) {
          this.escalateWasmPoisoning(error);
          if (this.env.ENVIRONMENT === "staging") {
            const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", seed));
            return json4({
              error: "invalid_snapshot",
              validationStage,
              seedBytes: seed.byteLength,
              sha256: Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(""),
              reason: (error instanceof Error ? error.stack ?? `${error.name}: ${error.message}` : String(error)).slice(0, 512)
            }, 400);
          }
          return json4({ error: "invalid_snapshot" }, 400);
        }
      }
      const before = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n;
      this.ctx.storage.transactionSync(() => {
        this.dropLog();
        if (seed.byteLength > 0) {
          this.blobs.put("snapshot", seed);
          this.setMeta("postReset", "0");
          this.setMeta("tailDirty", "1");
          this.setMeta("backupDirty", "1");
        }
        this.setMeta("replayAttempts", "0");
      });
      this.doc?.free();
      this.doc = void 0;
      this.closeSocketsForRoomReset();
      return json4({ ok: true, clearedUpdateRows: before ?? 0, seedBytes: seed.byteLength });
    }
    return new Response("not found", { status: 404 });
  }
  // ── WebSocket protocol ────────────────────────────────────────────────────
  async revokeGrant(grantId) {
    this.revokedGrants.add(grantId);
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment();
      if (state?.grantId !== grantId) continue;
      try {
        ws.close(4403, "device grant revoked");
      } catch {
      }
    }
  }
  async authorizeSocket(ws, state) {
    return enforceDeviceGrantAuthority(ws, state, Date.now(), async (grantId) => {
      if (this.revokedGrants.has(grantId)) return false;
      const stub = this.env.AUTH_GRANTS.get(
        this.env.AUTH_GRANTS.idFromName(grantId)
      );
      const response = await stub.fetch(
        new Request(`https://grant.internal/status?grantId=${encodeURIComponent(grantId)}`, {
          headers: { [GRANT_EVENT_HEADER]: "status" }
        })
      );
      return response.ok && !this.revokedGrants.has(grantId);
    });
  }
  async webSocketMessage(ws, message) {
    const attached = ws.deserializeAttachment();
    if (!await this.authorizeSocket(ws, attached)) return;
    if (typeof message === "string") return;
    const state = ws.deserializeAttachment();
    let decoded;
    try {
      decoded = decode(new Uint8Array(message));
    } catch {
      ws.close(1002, "Protocol error");
      return;
    }
    try {
      switch (decoded.type) {
        case MessageType.JoinRequest:
          await this.handleJoin(ws, state, decoded);
          break;
        case MessageType.DocUpdate:
          await this.handleDocUpdate(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragmentHeader:
          this.handleFragmentHeader(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragment:
          await this.handleFragment(ws, state, decoded);
          break;
        case MessageType.Leave:
          state.rooms = state.rooms.filter((r) => r !== decoded.crdt);
          ws.serializeAttachment(state);
          break;
        case MessageType.Ack:
        case MessageType.RoomError:
          break;
        default:
          ws.close(1002, "Unsupported message");
      }
    } catch (e) {
      console.error(
        "ws message handler failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state?.deviceId ?? "unattributed"}`,
        `type=${decoded.type}`,
        String(e)
      );
      if (decoded.type === MessageType.JoinRequest) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: decoded.crdt,
          roomId: decoded.roomId,
          code: JoinErrorCode.AppError,
          message: "internal error"
        });
      }
      this.escalateWasmPoisoning(e);
    }
  }
  async webSocketClose(ws) {
    this.logSocketEnd(ws, "closed");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket close failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }
  async webSocketError(ws) {
    this.logSocketEnd(ws, "errored");
    this.fragments.delete(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket error failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }
  /** RangeError("Invalid array buffer length") / wasm RuntimeError are the
   * signature of an exhausted loro-wasm heap (see `wasmPoisonStrikes`).
   * Strike out and `ctx.abort()` so clients redial into a fresh isolate
   * within seconds instead of hot-looping against a deaf room until
   * Cloudflare's memory-limit reset finally fires. */
  escalateWasmPoisoning(e) {
    if (this.env.ENVIRONMENT === "staging") {
      console.error(
        "Loro operation failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        (e instanceof Error ? e.stack ?? `${e.name}: ${e.message}` : String(e)).slice(0, 512)
      );
    }
    if (!(e instanceof RangeError || e instanceof WebAssembly.RuntimeError)) return;
    wasmPoisonStrikes++;
    if (wasmPoisonStrikes < WASM_POISON_ABORT_AFTER) return;
    console.error(`wasm heap poisoned (${wasmPoisonStrikes} strikes); aborting isolate for a fresh heap`);
    try {
      this.doc?.free();
    } catch {
    }
    this.doc = void 0;
    this.ctx.abort("loro-wasm heap exhausted; recycling isolate");
  }
  logSocketEnd(ws, how) {
    const state = ws.deserializeAttachment();
    console.log(
      `socket ${how}`,
      `room=${this.getMeta("chatId") ?? "?"}`,
      `device=${state?.deviceId ?? "unattributed"}`,
      `sockets=${Math.max(0, this.ctx.getWebSockets().length - 1)}`
    );
  }
  async handleJoin(ws, state, message) {
    const owner = this.getMeta("owner");
    if (!owner) this.setMeta("owner", state.projectScope);
    else if (owner !== state.projectScope) {
      this.send(ws, {
        type: MessageType.JoinError,
        crdt: message.crdt,
        roomId: message.roomId,
        code: JoinErrorCode.AuthFailed,
        message: "scope does not own this room"
      });
      return;
    }
    if (!this.getMeta("chatId") && message.roomId) this.setMeta("chatId", message.roomId);
    if (state.workspace || !this.getMeta("roomKind")) this.setMeta("roomKind", state.workspace ? "workspace" : "session");
    if (message.crdt === CrdtType.Loro) {
      await this.ensureDoc();
      if (!await this.authorizeSocket(ws, state)) return;
      if (ws.readyState !== WebSocket.OPEN) return;
      state = ws.deserializeAttachment();
      const doc = this.doc;
      if (!doc) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AppError,
          message: "document released; rejoin"
        });
        return;
      }
      try {
        this.assertLoroMaterialized(doc, this.docPendingEnds);
      } catch {
        state.rooms = state.rooms.filter((room) => room !== CrdtType.Loro);
        state.loroRecoveryRoomId = state.durableSync === true && canPublish(state.capabilities) ? message.roomId : void 0;
        ws.serializeAttachment(state);
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AppError,
          message: "incomplete_history"
        });
        return;
      }
      state.loroRecoveryRoomId = void 0;
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      const vv = doc.oplogVersion();
      try {
        this.send(ws, {
          type: MessageType.JoinResponseOk,
          crdt: message.crdt,
          roomId: message.roomId,
          permission: state.durableSync === true && canPublish(state.capabilities) ? "write" : "read",
          version: vv.encode()
        });
      } finally {
        vv.free();
      }
      let backfill;
      if (message.version.length > 0) {
        let from;
        let retainedSince;
        try {
          from = VersionVector.decode(message.version);
          retainedSince = doc.shallowSinceVV();
          const coverage = from.compare(retainedSince);
          if (from.length() > 0 && coverage !== void 0 && coverage >= 0) {
            backfill = doc.export({ mode: "update", from });
          }
        } catch {
        } finally {
          from?.free();
          retainedSince?.free();
        }
      }
      if (backfill) {
        if (backfill.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      } else {
        const baseline = this.blobs.get("snapshot");
        if (baseline?.length && !this.sendUpdates(ws, message.crdt, message.roomId, [baseline])) return;
        for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
          if (!this.sendUpdates(ws, message.crdt, message.roomId, [new Uint8Array(row.bytes)])) return;
        }
      }
      wasmPoisonStrikes = 0;
      return;
    }
    if (message.crdt === CrdtType.LoroEphemeralStore) {
      const eph = state.workspace ? void 0 : this.ensureEph();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: new Uint8Array()
      });
      if (state.workspace) {
        const cached = this.workspacePresenceSnapshot(state.deviceId, Date.now());
        if (cached.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, cached);
      } else if (eph) {
        const all = eph.encodeAll();
        if (all.length > 0) this.sendUpdates(ws, message.crdt, message.roomId, [all]);
      }
      return;
    }
    this.send(ws, {
      type: MessageType.JoinError,
      crdt: message.crdt,
      roomId: message.roomId,
      code: JoinErrorCode.Unknown,
      message: "unsupported crdt"
    });
  }
  async handleDocUpdate(ws, state, message) {
    if (message.updates.some((u) => u.length > MAX_MESSAGE_SIZE)) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge);
      return;
    }
    if (!state.rooms.includes(message.crdt) && !(message.crdt === CrdtType.Loro && state.loroRecoveryRoomId === message.roomId)) {
      console.warn(
        "update from non-member socket",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state.deviceId ?? "unattributed"}`,
        `crdt=${message.crdt}`
      );
      this.ack(ws, message, UpdateStatusCode.PermissionDenied);
      return;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, message.updates);
  }
  /** Shared apply path for whole and reassembled updates. */
  async applyUpdates(ws, state, crdt, roomId, batchId, updates) {
    if (crdt === CrdtType.Loro && (state.durableSync !== true || !canPublish(state.capabilities))) {
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.PermissionDenied, batchId);
      return;
    }
    if (crdt === CrdtType.Loro) {
      await this.ensureDoc();
      if (!await this.authorizeSocket(ws, state)) return;
      if (ws.readyState !== WebSocket.OPEN) return;
      let doc = this.doc;
      if (!doc) {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.workspace) this.notifyWorkspace(doc, state.projectScope, true);
      let repair;
      try {
        if (state.loroRecoveryRoomId !== void 0) {
          if (state.loroRecoveryRoomId !== roomId || updates.length !== 1 || updates[0].length === 0) {
            throw new Error("recovery requires a complete snapshot");
          }
          const metadata = decodeImportBlobMeta(updates[0], false);
          try {
            if (metadata.mode !== "snapshot" && metadata.mode !== "shallow-snapshot" && metadata.mode !== "outdated-snapshot") {
              throw new Error("recovery requires a complete snapshot");
            }
          } finally {
            metadata.partialStartVersionVector.free();
            metadata.partialEndVersionVector.free();
          }
        }
        ({ doc, repair } = this.importLoroUpdates(doc, updates));
      } catch (error) {
        this.escalateWasmPoisoning(error);
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.workspace) this.notifyWorkspace(doc, state.projectScope);
      await this.ctx.storage.sync();
      await Promise.all([
        this.relay(ws, crdt, roomId, repair ? [...updates, repair] : updates),
        (async () => {
          if (await this.authorizeSocket(ws, state) && ws.readyState === WebSocket.OPEN) {
            this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
            if (repair) this.sendUpdates(ws, crdt, roomId, [repair]);
          }
        })()
      ]);
      return;
    }
    if (crdt === CrdtType.LoroEphemeralStore) {
      if (updates.some((update) => update.length > MAX_PRESENCE_UPDATE_BYTES)) {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.PayloadTooLarge, batchId);
        return;
      }
      if (state.workspace) {
        this.cacheWorkspacePresence(state.deviceId, updates, Date.now());
      }
      if (!state.workspace) {
        const eph = this.ensureEph();
        try {
          for (const update of updates) if (update.length > 0) eph.apply(update);
        } catch {
          this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
          return;
        }
      }
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      await this.relay(ws, crdt, roomId, updates);
      return;
    }
    this.ack(ws, { crdt, roomId }, UpdateStatusCode.Unknown, batchId);
  }
  /** Legacy publishers can re-append an accepted id with fresh CRDT ancestry.
   * Validate immediately, but do not shift a positional 0.1.135 writer until
   * its real stream/session state is terminal. No timer or placeholder rows. */
  reconcileMessageIdentities(doc, retained = /* @__PURE__ */ new Set(), acceptedRuns = /* @__PURE__ */ new Set()) {
    if (this.getMeta("roomKind") === "workspace" || !doc.getShallowValue().messages) return {};
    const pending = new Map(JSON.parse(this.getMeta("messageIdentityRepairs") ?? "[]"));
    const next = /* @__PURE__ */ new Map();
    const messages = doc.getList("messages");
    const seen = /* @__PURE__ */ new Map();
    const remove = [];
    let lastStreaming = -1;
    const rowAt = /* @__PURE__ */ __name((index) => {
      const row = messages.get(index);
      try {
        return row instanceof LoroMap ? row.toJSON() : void 0;
      } finally {
        if (isContainer(row)) row.free();
      }
    }, "rowAt");
    try {
      for (let index = 0; index < messages.length; index++) {
        const row = messages.get(index);
        let id;
        try {
          if (row instanceof LoroMap) {
            id = row.get("id");
            if (typeof id === "string" && id) {
              const indices = seen.get(id) ?? [];
              indices.push({ index, containerId: row.id });
              seen.set(id, indices);
            }
            const status = row.get("status");
            if (status === "streaming" || isContainer(status)) lastStreaming = index;
            if (isContainer(status)) status.free();
          }
        } finally {
          if (isContainer(row)) row.free();
        }
        if (isContainer(id)) id.free();
      }
      for (const [id, indices] of seen) {
        if (indices.length < 2) continue;
        const anchor = pending.get(id);
        const originalRow = anchor ? indices.find((row) => row.containerId === anchor.containerId) : indices.find((row) => retained.has(row.containerId)) ?? indices[0];
        if (!originalRow) throw new Error(`message ${id} lost its accepted identity; originals retained`);
        const original = rowAt(originalRow.index);
        const { createdAt: originalAt, ...originalIdentity } = original;
        const fingerprint = createHash("sha256").update(JSON.stringify(original, (_key, value) => value && typeof value === "object" && !Array.isArray(value) ? Object.fromEntries(Object.entries(value).sort(([left], [right]) => left < right ? -1 : left > right ? 1 : 0)) : value)).digest("hex");
        let timestampEcho = anchor?.timestampEcho === true;
        for (const row of indices) {
          if (row === originalRow) continue;
          const { createdAt: retryAt, ...retryIdentity } = rowAt(row.index);
          const anchored = anchor !== void 0 || retained.has(originalRow.containerId) && !retained.has(row.containerId);
          if (!Array.isArray(original.parts) || typeof original.deviceId !== "string" || !Number.isSafeInteger(originalAt) || !Number.isSafeInteger(retryAt) || !isDeepStrictEqual(originalIdentity, retryIdentity) || anchor && anchor.fingerprint !== fingerprint) {
            throw new Error(`message ${id} names conflicting accepted content; originals retained`);
          }
          if (originalAt !== retryAt) {
            if (!(anchored && original.role === "user" && original.continuationOf == null && original.peerMessage == null && (timestampEcho || this.attestRetainedUserEcho(originalRow.containerId, original, acceptedRuns)))) {
              throw new Error(`message ${id} names conflicting accepted content; originals retained`);
            }
            timestampEcho = true;
          }
          remove.push(row.index);
        }
        next.set(id, { containerId: originalRow.containerId, fingerprint, timestampEcho });
      }
      if (!remove.length) return pending.size ? { deferred: "[]" } : {};
      if (lastStreaming >= remove.reduce((first, index) => Math.min(first, index), messages.length) || !this.sessionWritersQuiescent(doc)) {
        return { deferred: JSON.stringify([...next]) };
      }
      const before = doc.oplogVersion();
      try {
        for (const index of remove.sort((a, b) => b - a)) messages.delete(index, 1);
        doc.commit();
        return { repair: doc.export({ mode: "update", from: before }), deferred: "[]" };
      } finally {
        before.free();
      }
    } finally {
      messages.free();
    }
  }
  /** Both legacy append-only owner status and native agentSessions registers
   * must be terminal before retiring raw list positions used by old writers. */
  sessionWritersQuiescent(doc) {
    const sessions = /* @__PURE__ */ new Map();
    const observe = /* @__PURE__ */ __name((record) => {
      if (!record || typeof record !== "object") return;
      const publication = record;
      const value = publication.value?.value;
      if (publication.value?.kind !== "agentSession" || !value || typeof value.sessionId !== "string" || typeof value.ownerSubject !== "string" || typeof value.ownerDeviceId !== "string" || publication.publishedBy !== value.ownerSubject) return;
      const at = value.updatedAt ?? value.createdAt;
      if (typeof at !== "number" || !Number.isSafeInteger(at)) return;
      const previous = sessions.get(value.sessionId);
      if (previous && (previous.ownerSubject !== value.ownerSubject || previous.ownerDeviceId !== value.ownerDeviceId || previous.at > at)) return;
      sessions.set(value.sessionId, { ownerSubject: value.ownerSubject, ownerDeviceId: value.ownerDeviceId, status: value.status, at });
    }, "observe");
    const roots = doc.getShallowValue();
    if (roots.publications) {
      const publications = doc.getList("publications");
      try {
        for (let index = 0; index < publications.length; index++) {
          const row = publications.get(index);
          try {
            if (row instanceof LoroMap) {
              const record = row.get("record");
              try {
                observe(record);
              } finally {
                if (isContainer(record)) record.free();
              }
            }
          } finally {
            if (isContainer(row)) row.free();
          }
        }
      } finally {
        publications.free();
      }
    }
    if (roots.agentSessions) {
      const register = doc.getMap("agentSessions");
      try {
        for (const record of Object.values(register.toJSON())) observe(record);
      } finally {
        register.free();
      }
    }
    return [...sessions.values()].every(({ status }) => status === "idle" || status === "errored");
  }
  /** Capture only compact command/container identity before import. In particular,
   * never materialize RunRequest attachments on every streaming/metadata tick. */
  acceptedRunContainers(doc) {
    const accepted = /* @__PURE__ */ new Set();
    if (!doc.getShallowValue().commands) return accepted;
    const commands = doc.getList("commands");
    try {
      for (let index = 0; index < commands.length; index++) {
        const row = commands.get(index);
        try {
          if (row instanceof LoroMap) {
            const status = row.get("status");
            const kind = row.get("kind");
            try {
              if (status === "applied" && kind === "run") accepted.add(row.id);
            } finally {
              if (isContainer(status)) status.free();
              if (isContainer(kind)) kind.free();
            }
          }
        } finally {
          if (isContainer(row)) row.free();
        }
      }
      return accepted;
    } finally {
      commands.free();
    }
  }
  /** Only a newly encountered timestamp echo needs this read. Reconstruct the
   * already-durable pre-import state, never the sender's rewritten command.
   * The persisted fingerprint/proof makes subsequent ticks and restarts cheap. */
  attestRetainedUserEcho(containerId, original, acceptedRuns) {
    if (!acceptedRuns.size) return false;
    const accepted = new LoroDoc();
    try {
      const baseline = this.blobs.get("snapshot");
      if (baseline?.length) accepted.import(baseline);
      this.replayLog(accepted);
      this.assertLoroMaterialized(accepted);
      const message = accepted.getContainerById(containerId);
      try {
        if (!(message instanceof LoroMap) || !isDeepStrictEqual(message.toJSON(), original)) return false;
      } finally {
        if (isContainer(message)) message.free();
      }
      for (const id of acceptedRuns) {
        const row = accepted.getContainerById(id);
        try {
          if (!(row instanceof LoroMap) || row.get("status") !== "applied") continue;
          const payload = row.get("payload");
          if (!payload || typeof payload !== "object" || isContainer(payload)) {
            if (isContainer(payload)) payload.free();
            continue;
          }
          const command = payload;
          if (command.kind === "run" && command.messageId === original.id && typeof command.request?.prompt === "string" && isDeepStrictEqual(original.parts, [{ id: "t0", kind: "text", text: command.request.prompt }])) return true;
        } finally {
          if (isContainer(row)) row.free();
        }
      }
      return false;
    } finally {
      accepted.free();
    }
  }
  acceptedMessageContainers(doc) {
    if (this.getMeta("roomKind") === "workspace" || !doc.getShallowValue().messages) return /* @__PURE__ */ new Set();
    const messages = doc.getList("messages");
    try {
      return new Set(messages.getShallowValue());
    } finally {
      messages.free();
    }
  }
  importLoroUpdates(doc, updates) {
    if (updates.every((update) => update.length === 0)) {
      this.assertLoroMaterialized(doc, this.docPendingEnds);
      return { doc };
    }
    const snapshotIndex = updates.findIndex((update) => {
      if (update.length === 0) return false;
      const metadata = decodeImportBlobMeta(update, false);
      try {
        return metadata.mode === "snapshot" || metadata.mode === "shallow-snapshot" || metadata.mode === "outdated-snapshot";
      } finally {
        metadata.partialStartVersionVector.free();
        metadata.partialEndVersionVector.free();
      }
    });
    const retained = this.acceptedMessageContainers(doc);
    const session = this.getMeta("roomKind") !== "workspace";
    const acceptedRuns = session ? this.acceptedRunContainers(doc) : /* @__PURE__ */ new Set();
    if (snapshotIndex < 0) {
      const before = doc.oplogFrontiers();
      const retainedPending = this.docPendingEnds;
      let pendingEnds2 = retainedPending ? new Map(retainedPending) : void 0;
      try {
        const imported = doc.importBatch(updates.filter((update) => update.length > 0));
        for (const [peer, span] of imported.pending ?? []) {
          pendingEnds2 ??= /* @__PURE__ */ new Map();
          pendingEnds2.set(peer, Math.max(pendingEnds2.get(peer) ?? 0, span.end));
        }
        this.assertLoroMaterialized(doc, pendingEnds2);
        const { repair, deferred } = this.reconcileMessageIdentities(doc, retained, acceptedRuns);
        const after = doc.oplogFrontiers();
        const changed = before.length !== after.length || !before.every((head) => after.some((other) => head.peer === other.peer && head.counter === other.counter));
        if (changed || retainedPending?.size) {
          this.recordLoroUpdates(doc, repair ? [...updates, repair] : updates, Boolean(pendingEnds2?.size), deferred);
        }
        this.docPendingEnds = void 0;
        return { doc, repair };
      } catch (error) {
        this.doc = void 0;
        doc.free();
        let restored;
        try {
          restored = new LoroDoc();
          const baseline = this.blobs.get("snapshot");
          if (baseline?.length) restored.import(baseline);
          const pendingEnds3 = this.replayLog(restored).pendingEnds;
          this.assertLoroMaterialized(restored, pendingEnds3);
          this.doc = restored;
          this.docPendingEnds = retainedPending;
        } catch (rollbackError) {
          this.doc = void 0;
          restored?.free();
          throw new AggregateError([error, rollbackError], "Loro update rollback failed; accepted bytes retained");
        }
        throw error;
      }
    }
    let candidate = new LoroDoc();
    let ownsCandidate = true;
    let previousVersion;
    let pendingEnds;
    const replay = /* @__PURE__ */ __name((update) => {
      if (update.length === 0) return;
      const imported = candidate.import(update);
      for (const [peer, span] of imported.pending ?? []) {
        pendingEnds ??= /* @__PURE__ */ new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
      }
    }, "replay");
    try {
      previousVersion = doc.oplogVersion();
      let baseline = updates[snapshotIndex];
      if (baseline?.length) replay(baseline);
      if (snapshotIndex >= 0) {
        this.assertLoroMaterialized(candidate, pendingEnds);
        const candidateVersion = candidate.oplogVersion();
        try {
          const coverage = candidateVersion.compare(previousVersion);
          if (coverage !== void 0 && coverage <= 0) {
            candidate.free();
            ownsCandidate = false;
            return this.importLoroUpdates(doc, updates.filter((_, i) => i !== snapshotIndex));
          }
          if (this.retainsWorkspaceHistory() && (previousVersion.length() > 0 || this.docPendingEnds?.size)) {
            const incomingFloor = candidate.shallowSinceVV();
            const retainedFloor = doc.shallowSinceVV();
            try {
              const floorCoverage = incomingFloor.compare(retainedFloor);
              if (floorCoverage === void 0 || floorCoverage >= 0) {
                const available = previousVersion.compare(incomingFloor);
                if (available === void 0 || available < 0) {
                  throw new Error("snapshot advances beyond retained-history boundary");
                }
                const delta = candidate.export({ mode: "update", from: previousVersion });
                candidate.free();
                ownsCandidate = false;
                return this.importLoroUpdates(doc, [delta, ...updates.filter((_, i) => i !== snapshotIndex)]);
              }
            } finally {
              retainedFloor.free();
              incomingFloor.free();
            }
          }
          if (coverage === void 0) {
            const retainedSince = doc.shallowSinceVV();
            try {
              const retainedCoverage = candidateVersion.compare(retainedSince);
              if (retainedCoverage === void 0 || retainedCoverage < 0) {
                throw new Error("snapshot does not cover retained-history boundary");
              }
            } finally {
              retainedSince.free();
            }
            replay(doc.export({ mode: "update", from: candidateVersion }));
          }
        } finally {
          candidateVersion.free();
        }
      }
      if (this.docPendingEnds?.size) {
        const retained2 = this.replayLog(candidate).pendingEnds;
        for (const [peer, end] of this.docPendingEnds) {
          pendingEnds ??= /* @__PURE__ */ new Map();
          pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
        }
        for (const [peer, end] of retained2 ?? []) {
          pendingEnds ??= /* @__PURE__ */ new Map();
          pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
        }
      }
      for (let i = 0; i < updates.length; i++) {
        if (i !== snapshotIndex) replay(updates[i]);
      }
      this.assertLoroMaterialized(candidate, pendingEnds);
      const mergedVersion = candidate.oplogVersion();
      try {
        const coverage = mergedVersion.compare(previousVersion);
        if (coverage === void 0 || coverage < 0) throw new Error("import lost retained operations");
      } finally {
        mergedVersion.free();
      }
      const { repair, deferred } = this.reconcileMessageIdentities(candidate, retained, acceptedRuns);
      this.recordLoroUpdates(candidate, repair ? [...updates, repair] : updates, true, deferred);
      this.doc = candidate;
      this.docPendingEnds = void 0;
      ownsCandidate = false;
      doc.free();
      return { doc: candidate, repair };
    } finally {
      previousVersion?.free();
      if (ownsCandidate) candidate.free();
    }
  }
  assertLoroMaterialized(doc, pendingEnds) {
    if (pendingEnds?.size) {
      const version = doc.oplogVersion();
      try {
        for (const [peer, end] of pendingEnds) {
          if ((version.get(peer) ?? 0) < end) throw new Error("import has unresolved dependencies");
        }
      } finally {
        version.free();
      }
    }
    const state = doc.frontiers();
    const oplog = doc.oplogFrontiers();
    if (doc.isDetached() || state.length !== oplog.length || !state.every((head) => oplog.some((other) => head.peer === other.peer && head.counter === other.counter))) {
      throw new Error("import did not materialize complete state");
    }
  }
  workspaceNotifications() {
    return this.notifications ??= new WorkspaceNotifications(this.ctx.storage, this.env);
  }
  notifyWorkspace(doc, projectScope, baseline = false) {
    let phase = "workspace_notifications_construct";
    try {
      const notifications = this.workspaceNotifications();
      phase = "observe";
      const events = notifications.observe(doc, baseline);
      phase = "schedule_delivery";
      if (events.length) {
        this.ctx.waitUntil(notifications.deliver(events, projectScope).catch(() => {
          console.warn("Crew push delivery failed");
        }));
      }
    } catch (error) {
      if (this.env.ENVIRONMENT === "staging") console.warn("Crew push observation failed", phase, (error instanceof Error ? error.message : String(error)).slice(0, 512));
      else console.warn("Crew push observation failed");
    }
  }
  /** Commit accepted bytes and fold status churn without discarding CRDT history.
   * No accepted payload remains in a volatile JS buffer after this returns. */
  recordLoroUpdates(doc, updates, forceSnapshot = false, deferred) {
    const bytes = updates.reduce((total, update) => total + update.byteLength, 0);
    if (!bytes && deferred === void 0) return;
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + bytes;
    const rows = Number([...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n ?? 0);
    const fold = forceSnapshot || logBytes >= COMPACT_LOG_BYTES || rows + updates.length >= COMPACT_LOG_ROWS || updates.some((update) => update.byteLength > CHUNK_BYTES);
    const snapshot = fold ? doc.export({ mode: "snapshot" }) : void 0;
    this.ctx.storage.transactionSync(() => {
      if (snapshot) {
        this.blobs.put("snapshot", snapshot);
        this.ctx.storage.sql.exec("DELETE FROM updates");
        this.setMeta("updateBytes", "0");
      } else {
        const now = Date.now();
        for (const update of updates) {
          if (!update.byteLength) continue;
          const value = update.byteOffset === 0 && update.byteLength === update.buffer.byteLength ? update.buffer : update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength);
          this.ctx.storage.sql.exec("INSERT INTO updates (bytes, received_at) VALUES (?, ?)", value, now);
        }
        this.setMeta("updateBytes", String(logBytes));
      }
      if (deferred !== void 0 && this.getMeta("messageIdentityRepairs") !== deferred) this.setMeta("messageIdentityRepairs", deferred);
      this.setMeta("tailDirty", "1");
      this.setMeta("backupDirty", "1");
      this.setMeta("postReset", "0");
    });
    this.markActivity();
  }
  pruneFragments(now) {
    for (const [socket, batches] of this.fragments) {
      for (const [id, batch] of batches) if (batch.expiresAt <= now) batches.delete(id);
      if (!batches.size) this.fragments.delete(socket);
    }
    clearTimeout(this.fragmentTimer);
    this.fragmentTimer = void 0;
    if (this.fragments.size) {
      const expiry = Math.min(...[...this.fragments.values()].flatMap((batches) => [...batches.values()].map((batch) => batch.expiresAt)));
      this.fragmentTimer = setTimeout(() => this.pruneFragments(Date.now()), Math.max(1, expiry - now));
    }
  }
  handleFragmentHeader(ws, state, message) {
    if (message.crdt === CrdtType.Loro && (state.durableSync !== true || !canPublish(state.capabilities))) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    if (!state.rooms.includes(message.crdt) && !(message.crdt === CrdtType.Loro && state.loroRecoveryRoomId === message.roomId)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    this.pruneFragments(Date.now());
    let batches = this.fragments.get(ws);
    if (!Number.isSafeInteger(message.fragmentCount) || message.fragmentCount <= 0 || message.fragmentCount > MAX_FRAGMENT_COUNT || !Number.isSafeInteger(message.totalSizeBytes) || message.totalSizeBytes < 0 || message.totalSizeBytes > MAX_REASSEMBLED_BYTES) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    let reservedBytes = message.totalSizeBytes + this.applyingFragmentBytes;
    let reservedParts = message.fragmentCount + this.applyingFragmentParts;
    for (const [socket, inFlight] of this.fragments) {
      for (const [id, batch] of inFlight) {
        if (socket === ws && id === message.batchId) continue;
        reservedBytes += batch.totalSize;
        reservedParts += batch.parts.length;
      }
    }
    if (reservedBytes > MAX_REASSEMBLED_BYTES || reservedParts > MAX_FRAGMENT_COUNT) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    if (!batches) {
      batches = /* @__PURE__ */ new Map();
      this.fragments.set(ws, batches);
    }
    batches.set(message.batchId, {
      parts: new Array(message.fragmentCount),
      received: 0,
      receivedBytes: 0,
      totalSize: message.totalSizeBytes,
      header: message,
      expiresAt: Date.now() + FRAGMENT_TTL_MS
    });
    this.pruneFragments(Date.now());
  }
  async handleFragment(ws, state, message) {
    if (message.crdt === CrdtType.Loro && (state.durableSync !== true || !canPublish(state.capabilities))) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    const batch = this.fragments.get(ws)?.get(message.batchId);
    if (!batch || batch.expiresAt <= Date.now()) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.FragmentTimeout, message.batchId);
      return;
    }
    if (message.crdt !== batch.header.crdt || message.roomId !== batch.header.roomId || !Number.isSafeInteger(message.index) || message.index < 0 || message.index >= batch.parts.length) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
      return;
    }
    if (batch.parts[message.index] !== void 0) return;
    if (message.fragment.length > batch.totalSize - batch.receivedBytes) {
      this.fragments.get(ws)?.delete(message.batchId);
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    batch.parts[message.index] = message.fragment.slice();
    batch.received++;
    batch.receivedBytes += message.fragment.length;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    if (batch.receivedBytes !== batch.totalSize) {
      this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
      return;
    }
    this.applyingFragmentBytes += batch.totalSize;
    this.applyingFragmentParts += batch.parts.length;
    this.pruneFragments(Date.now());
    try {
      const total = new Uint8Array(batch.totalSize);
      let off = 0;
      for (let i = 0; i < batch.parts.length; i++) {
        const part = batch.parts[i];
        if (part === void 0) throw new Error("incomplete fragment batch");
        total.set(part, off);
        off += part.length;
        batch.parts[i] = void 0;
      }
      await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
    } finally {
      this.applyingFragmentBytes -= batch.totalSize;
      this.applyingFragmentParts -= batch.parts.length;
    }
  }
  // ── doc/ephemeral materialization ────────────────────────────────────────
  async ensureDoc() {
    this.touchDoc();
    if (this.docLoad) return this.docLoad;
    if (this.doc) return this.doc;
    this.docLoad = this.materializeDoc();
    try {
      return await this.docLoad;
    } finally {
      this.docLoad = void 0;
    }
  }
  /** Materialize the complete journal union after bootstrapping its snapshot. */
  replayLog(doc) {
    let rows = 0;
    let pendingEnds;
    for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
      const imported = doc.import(new Uint8Array(row.bytes));
      rows++;
      for (const [peer, span] of imported.pending ?? []) {
        pendingEnds ??= /* @__PURE__ */ new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
      }
    }
    return { rows, batches: rows, pendingEnds };
  }
  async materializeDoc() {
    const started = Date.now();
    const attempts = Number(this.getMeta("replayAttempts") ?? "0");
    this.setMeta("replayAttempts", String(attempts + 1));
    await this.ctx.storage.sync();
    const snapshot = this.blobs.get("snapshot");
    const doc = new LoroDoc();
    let pendingEnds;
    const replay = /* @__PURE__ */ __name((bytes) => {
      const imported = Array.isArray(bytes) ? doc.importBatch(bytes) : doc.import(bytes);
      for (const [peer, span] of imported.pending ?? []) {
        pendingEnds ??= /* @__PURE__ */ new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, span.end));
      }
    }, "replay");
    if (snapshot?.length) {
      try {
        replay(snapshot);
      } catch (error) {
        return await this.rejectPersistedLoroState(doc, "snapshot", error);
      }
    }
    let rows = 0;
    let batches = 0;
    let retained = /* @__PURE__ */ new Set();
    let acceptedRuns = /* @__PURE__ */ new Set();
    try {
      retained = this.acceptedMessageContainers(doc);
      if (this.getMeta("roomKind") !== "workspace") acceptedRuns = this.acceptedRunContainers(doc);
      const replayed = this.replayLog(doc);
      rows = replayed.rows;
      batches = replayed.batches;
      for (const [peer, end] of replayed.pendingEnds ?? []) {
        pendingEnds ??= /* @__PURE__ */ new Map();
        pendingEnds.set(peer, Math.max(pendingEnds.get(peer) ?? 0, end));
      }
    } catch (error) {
      return await this.rejectPersistedLoroState(doc, "update journal", error);
    }
    this.doc = doc;
    if (pendingEnds?.size) {
      const version = doc.oplogVersion();
      try {
        for (const [peer, end] of pendingEnds) {
          if ((version.get(peer) ?? 0) >= end) pendingEnds.delete(peer);
        }
      } finally {
        version.free();
      }
    }
    this.docPendingEnds = pendingEnds?.size ? pendingEnds : void 0;
    if (!this.docPendingEnds) {
      try {
        const { repair, deferred } = this.reconcileMessageIdentities(doc, retained, acceptedRuns);
        if (repair || deferred !== void 0) this.recordLoroUpdates(doc, repair ? [repair] : [], false, deferred);
      } catch (error) {
        this.doc = void 0;
        return await this.rejectPersistedLoroState(doc, "message identities", error);
      }
    }
    this.setMeta("replayAttempts", "0");
    await this.ctx.storage.sync();
    const replayMs = Date.now() - started;
    this.setMeta("lastReplayMs", String(replayMs));
    this.setMeta("lastReplayRows", String(rows));
    this.setMeta("lastReplayBatches", String(batches));
    console.log(
      `cold replay: ${replayMs}ms, ${rows} rows, snapshot ${snapshot?.length ?? 0}B, attempt ${attempts + 1}`,
      `room=${this.getMeta("chatId") ?? "?"}`
    );
    if (rows >= COMPACT_LOG_ROWS && !this.docPendingEnds) {
      this.assertLoroMaterialized(doc);
      const baseline = doc.export({ mode: "snapshot" });
      this.ctx.storage.transactionSync(() => {
        this.blobs.put("snapshot", baseline);
        this.ctx.storage.sql.exec("DELETE FROM updates");
        this.setMeta("updateBytes", "0");
      });
      await this.ctx.storage.sync();
    }
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]");
    const newest = checkpoints[checkpoints.length - 1];
    if (!newest || Date.now() - newest.at >= DAY_MS) {
      checkpoints.push({
        at: Date.now(),
        frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
      });
      while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();
      this.setMeta("checkpoints", JSON.stringify(checkpoints));
    }
    if (await this.trimHistoryIfDue(doc, Date.now())) {
      console.log(`history trimmed on cold start room=${this.getMeta("chatId") ?? "?"}`);
    }
    this.setMeta("lastColdMs", String(Date.now() - started));
    return this.doc;
  }
  /** Free idle WASM handles; accepted history already lives in SQL. */
  touchDoc() {
    this.lastDocUse = Date.now();
    if (this.docIdleTimer) return;
    this.docIdleTimer = setTimeout(() => this.releaseIdleDoc(), DOC_IDLE_RELEASE_MS + 500);
  }
  releaseIdleDoc() {
    this.docIdleTimer = void 0;
    if (!this.doc && !this.eph) return;
    const idle = Date.now() - this.lastDocUse;
    if (idle < DOC_IDLE_RELEASE_MS || this.docLoad) {
      this.docIdleTimer = setTimeout(
        () => this.releaseIdleDoc(),
        Math.max(DOC_IDLE_RELEASE_MS - idle, 1e3) + 500
      );
      return;
    }
    this.doc?.free();
    this.doc = void 0;
    this.eph?.destroy();
    this.eph = void 0;
    this.workspacePresence.clear();
  }
  closeSocketsForRoomReset() {
    for (const sock of this.ctx.getWebSockets()) {
      try {
        sock.close(4410, "room reset");
      } catch {
      }
    }
  }
  /** Failed replay retains accepted bytes; explicit administrator resets are
   * separate from automatic recovery. */
  async rejectPersistedLoroState(doc, source, error) {
    console.error(
      "persisted Loro replay rejected",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `source=${source}`,
      String(error)
    );
    try {
      doc.free();
    } catch {
    }
    this.doc = void 0;
    await this.ctx.storage.sync();
    this.closeSocketsForRoomReset();
    throw error;
  }
  /** Drop the persisted update log + snapshot (the /reset-log storage clear):
   * the next materialization starts empty and engines re-upload state on
   * rejoin. Preserves owner/chatId meta. */
  dropLog() {
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.blobs.delete("snapshot");
    this.setMeta("updateBytes", "0");
    this.setMeta("messageIdentityRepairs", "[]");
    this.setMeta("checkpoints", "[]");
    this.setMeta("lastTrimAt", "");
    this.docPendingEnds = void 0;
    this.setMeta("postReset", "1");
    this.setMeta("replayAttempts", "0");
  }
  ensureEph() {
    this.touchDoc();
    if (!this.eph) this.eph = new EphemeralStore(3e4);
    return this.eph;
  }
  pruneWorkspacePresence(now) {
    for (const [deviceId, entry] of this.workspacePresence) {
      if (entry.expiresAt <= now) this.workspacePresence.delete(deviceId);
    }
  }
  workspacePresenceSnapshot(deviceId, now) {
    this.pruneWorkspacePresence(now);
    const updates = [];
    for (const [peerDeviceId, entry] of this.workspacePresence) {
      if (peerDeviceId !== deviceId) updates.push(...entry.updates);
    }
    return updates;
  }
  cacheWorkspacePresence(deviceId, updates, now) {
    if (!deviceId) return;
    const totalBytes = updates.reduce((total, update) => total + update.byteLength, 0);
    if (totalBytes === 0 || totalBytes > MAX_PRESENCE_UPDATE_BYTES) return;
    this.pruneWorkspacePresence(now);
    this.workspacePresence.delete(deviceId);
    while (this.workspacePresence.size >= MAX_WORKSPACE_PRESENCE_PEERS) {
      const oldest = this.workspacePresence.keys().next().value;
      if (oldest === void 0) break;
      this.workspacePresence.delete(oldest);
    }
    this.workspacePresence.set(deviceId, {
      expiresAt: now + WORKSPACE_PRESENCE_TTL_MS,
      updates: updates.map((update) => update.slice())
    });
  }
  // ── durability: flush, compaction, backups ───────────────────────────────
  async flush() {
    if (!this.retainsWorkspaceHistory() && (this.blobs.byteLength("snapshot") ?? 0) > TRIM_FORCE_BYTES) {
      await this.foldLog();
    }
    await this.ctx.storage.sync();
  }
  /** Maintenance fold. Workspace folds are lossless; session trimming is unchanged. */
  async foldLog() {
    await this.ensureDoc();
    let doc = this.doc;
    if (!doc) throw new Error("document released during log fold");
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    if (await this.trimHistoryIfDue(doc, Date.now())) return;
    await this.ensureDoc();
    doc = this.doc;
    if (!doc) throw new Error("document released during log fold");
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    const snapshot = doc.export({ mode: "snapshot" });
    this.ctx.storage.transactionSync(() => {
      this.blobs.put("snapshot", snapshot);
      this.ctx.storage.sql.exec("DELETE FROM updates");
      this.setMeta("updateBytes", "0");
    });
    await this.ctx.storage.sync();
  }
  retainsWorkspaceHistory() {
    const kind = this.getMeta("roomKind");
    return kind === "workspace" || kind !== "session" && !canonicalSessionId(this.getMeta("chatId"));
  }
  /** HISTORY TRIM (§3.1): shallow snapshot at the newest recorded frontier
   * checkpoint older than RETAIN_DAYS — history before it is discarded
   * permanently, state fully preserved. Returns whether a trim landed (the
   * snapshot + log + materialized doc were all replaced — the passed `doc`
   * is CONSUMED: its wasm memory is freed, callers must switch to
   * `this.doc`). Best-effort: any export failure leaves the room to the
   * caller's lossless fold. */
  async trimHistoryIfDue(doc, now) {
    if (this.retainsWorkspaceHistory() || this.docPendingEnds?.size) return false;
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]");
    const retainedBytes = (this.blobs.byteLength("snapshot") ?? 0) + Number(this.getMeta("updateBytes") ?? "0");
    const cutoff = retainedBytes > TRIM_FORCE_BYTES ? void 0 : checkpoints.filter((c) => now - c.at >= RETAIN_MS).pop();
    let frontiers;
    if (cutoff && this.getMeta("lastTrimAt") !== String(cutoff.at)) {
      frontiers = cutoff.frontiers.map((f) => ({ peer: f.peer, counter: f.counter }));
    } else if (retainedBytes > TRIM_FORCE_BYTES) {
      frontiers = doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }));
    } else {
      return false;
    }
    try {
      const shallow = doc.export({
        mode: "shallow-snapshot",
        frontiers
      });
      const fresh = new LoroDoc();
      try {
        fresh.import(shallow);
        this.assertLoroMaterialized(fresh);
        this.ctx.storage.transactionSync(() => {
          this.blobs.put("snapshot", shallow);
          this.ctx.storage.sql.exec("DELETE FROM updates");
          this.setMeta("updateBytes", "0");
          this.setMeta("lastTrimAt", String(cutoff?.at ?? now));
        });
      } catch (error) {
        fresh.free();
        throw error;
      }
      this.doc = fresh;
      if (doc !== fresh) doc.free();
    } catch (error) {
      console.error("history trim failed", `room=${this.getMeta("chatId") ?? "?"}`, String(error));
      return false;
    }
    await this.ctx.storage.sync();
    return true;
  }
  closeExpiredGrantSockets(now) {
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment();
      if (!Number.isSafeInteger(state?.grantExpiresAt) || state?.grantExpiresAt > now) {
        continue;
      }
      try {
        ws.close(4403, "device grant expired");
      } catch {
      }
    }
  }
  async scheduleGrantExpiryAlarm() {
    const now = Date.now();
    let earliest;
    for (const ws of this.ctx.getWebSockets()) {
      const state = ws.deserializeAttachment();
      const expiresAt = state?.grantExpiresAt;
      if (!Number.isSafeInteger(expiresAt) || expiresAt <= now) continue;
      if (earliest === void 0 || expiresAt < earliest) {
        earliest = expiresAt;
      }
    }
    if (earliest === void 0) return;
    const scheduled = await this.ctx.storage.getAlarm();
    if (scheduled === null || earliest < scheduled) {
      await this.ctx.storage.setAlarm(earliest);
    }
  }
  /** Daily maintenance and device-grant expiry alarm. */
  async alarm() {
    const now = Date.now();
    this.closeExpiredGrantSockets(now);
    await this.flush();
    if (this.getMeta("backupDirty") !== "1") {
      await this.scheduleGrantExpiryAlarm();
      return;
    }
    await this.ensureDoc();
    const doc = this.doc;
    if (!doc) throw new Error("document released during maintenance");
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]");
    checkpoints.push({
      at: now,
      frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
    });
    while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();
    this.setMeta("checkpoints", JSON.stringify(checkpoints));
    await this.trimHistoryIfDue(doc, now);
    await this.ensureDoc();
    const chatId = this.getMeta("chatId");
    if (chatId && this.getMeta("postReset") !== "1" && !this.docPendingEnds?.size) {
      const current = this.doc;
      if (!current) throw new Error("document released during backup");
      this.assertLoroMaterialized(current, this.docPendingEnds);
      const prevVV = this.getMeta("backupVV");
      let advances = true;
      if (prevVV) {
        let prev;
        let cur;
        try {
          prev = VersionVector.decode(Uint8Array.from(atob(prevVV), (c) => c.charCodeAt(0)));
          cur = current.oplogVersion();
          const cmp = cur.compare(prev);
          advances = cmp !== void 0 && cmp >= 0;
        } catch {
        } finally {
          prev?.free();
          cur?.free();
        }
      }
      if (advances) {
        const snapshot = current.export({ mode: "snapshot" });
        const vv = current.oplogVersion();
        try {
          const encodedVersion = btoa(String.fromCharCode(...vv.encode()));
          await this.env.BLOBS.put(`backup/rooms/${this.ctx.id.toString()}/latest.loro`, snapshot);
          this.setMeta("backupVV", encodedVersion);
          const liveVersion = this.doc?.oplogVersion();
          try {
            if (liveVersion?.compare(vv) === 0 && !this.docPendingEnds?.size) {
              this.setMeta("backupDirty", "0");
            }
          } finally {
            liveVersion?.free();
          }
        } finally {
          vv.free();
        }
      }
    }
    await this.scheduleGrantExpiryAlarm();
  }
  /** Arm the daily alarm if none is scheduled (called on every write). */
  markActivity() {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }
  async currentTail() {
    await this.flush();
    if (this.getMeta("tailDirty") !== "1") {
      const cached = getJsonBlob(this.blobs, "tail");
      if (cached !== void 0) return cached;
    }
    const doc = await this.ensureDoc();
    this.assertLoroMaterialized(doc, this.docPendingEnds);
    const tail = materializeTail(doc, Date.now());
    putJsonBlob(this.blobs, "tail", tail);
    this.setMeta("tailDirty", "0");
    return tail;
  }
  // ── wire helpers ─────────────────────────────────────────────────────────
  /** Returns false when the frame could not be delivered (socket gone /
   * runtime refused the send). Encode failures throw out instead — they are
   * OUR bug, never the peer's, and must not be mistaken for a deaf socket. */
  send(ws, message) {
    const bytes = encode(message);
    try {
      ws.send(bytes);
      return true;
    } catch {
      return false;
    }
  }
  /** Send updates, fragmenting any single update above FRAGMENT_BYTES and
   * chunking small ones so no encoded frame approaches the loro-protocol
   * 256KB message cap (envelope overhead included). Returns false if any
   * frame failed to deliver. */
  sendUpdates(ws, crdt, roomId, updates) {
    let ok = true;
    let batch = [];
    let batchBytes = 0;
    const flushBatch = /* @__PURE__ */ __name(() => {
      if (batch.length === 0) return;
      ok = this.send(ws, {
        type: MessageType.DocUpdate,
        crdt,
        roomId,
        updates: batch,
        batchId: this.newBatchId()
      }) && ok;
      batch = [];
      batchBytes = 0;
    }, "flushBatch");
    for (const update of updates) {
      if (update.length <= FRAGMENT_BYTES) {
        if (batchBytes + update.length > FRAGMENT_BYTES) flushBatch();
        batch.push(update);
        batchBytes += update.length;
        continue;
      }
      flushBatch();
      const batchId = this.newBatchId();
      const fragmentCount = Math.ceil(update.length / FRAGMENT_BYTES);
      ok = this.send(ws, {
        type: MessageType.DocUpdateFragmentHeader,
        crdt,
        roomId,
        batchId,
        fragmentCount,
        totalSizeBytes: update.length
      }) && ok;
      for (let i = 0; i < fragmentCount; i++) {
        ok = this.send(ws, {
          type: MessageType.DocUpdateFragment,
          crdt,
          roomId,
          batchId,
          index: i,
          fragment: update.subarray(
            i * FRAGMENT_BYTES,
            Math.min((i + 1) * FRAGMENT_BYTES, update.length)
          )
        }) && ok;
      }
    }
    flushBatch();
    return ok;
  }
  /** Relay accepted updates to every other member socket via sendUpdates —
   * NOT a single pre-encoded frame. broadcast() used to encode the batch
   * once, so a reassembled >256KB client push (a device re-uploading its
   * full workspace history after a server reset) blew the loro-protocol
   * message cap and NEVER reached peers live; they only converged via a
   * later rejoin backfill (2026-08-04, the last silent-staleness path). */
  async relay(from, crdt, roomId, updates) {
    await Promise.all(this.ctx.getWebSockets().map(async (ws) => {
      if (ws === from) return;
      const state = ws.deserializeAttachment();
      if (!state?.rooms.includes(crdt)) return;
      if (!await this.authorizeSocket(ws, state) || ws.readyState !== WebSocket.OPEN) return;
      if (!ws.deserializeAttachment()?.rooms.includes(crdt)) return;
      if (!this.sendUpdates(ws, crdt, roomId, updates)) {
        console.warn(
          "relay send failed; closing socket",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `device=${state.deviceId ?? "unattributed"}`
        );
        try {
          ws.close(1011, "broadcast delivery failed");
        } catch {
        }
      }
    }));
  }
  ack(ws, message, status, refId) {
    this.send(ws, {
      type: MessageType.Ack,
      crdt: message.crdt,
      roomId: message.roomId,
      refId: refId ?? message.batchId ?? "0x0000000000000000",
      status
    });
  }
  newBatchId() {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }
};
var json4 = /* @__PURE__ */ __name((value, status = 200) => new Response(JSON.stringify(value), {
  status,
  headers: { "content-type": "application/json" }
}), "json");

// src/index.ts
var ID_RE4 = /^[A-Za-z0-9_-]{1,128}$/;
var SHA256_RE = /^[a-f0-9]{64}$/;
var MAX_ATTACHMENT_BYTES = 32 * 1024 * 1024;
var SANDBOX_DEVICE_PREFIX = "comet-scaffold-";
var RELEASE_FILE_RE = /^[A-Za-z0-9._-]{1,200}$/;
var json5 = /* @__PURE__ */ __name((value, status = 200) => new Response(JSON.stringify(value), {
  status,
  headers: { "content-type": "application/json" }
}), "json");
var requestInit = /* @__PURE__ */ __name((request) => ({
  method: request.method,
  body: request.body,
  duplex: "half"
}), "requestInit");
var forward = /* @__PURE__ */ __name((ns, name, request, identity, path, search = "", roomKind, deviceHostAuthorization) => {
  const stub = ns.get(ns.idFromName(name));
  const url = new URL(request.url);
  const declarations = url.searchParams.getAll(SYNC_PROTOCOL_QUERY);
  const durableSync = declarations.length === 1 && declarations[0] === DURABLE_SYNC_PROTOCOL;
  url.pathname = path;
  url.search = search;
  const headers = new Headers(request.headers);
  stripTrustedAuthHeaders(headers);
  headers.delete("authorization");
  if (durableSync) headers.set(SYNC_PROTOCOL_HEADER, DURABLE_SYNC_PROTOCOL);
  headers.set(AUTH_USER_HEADER, identity.userId);
  headers.set(AUTH_PROJECT_HEADER, identity.projectScope);
  headers.set(AUTH_CAPABILITIES_HEADER, identity.capabilities.join(" "));
  if (identity.credential === "device") {
    const grant = identity;
    headers.set(
      AUTH_GRANT_HEADER,
      JSON.stringify({
        grantId: grant.grantId,
        subject: grant.userId,
        scope: {
          projectId: grant.projectId,
          deploymentId: grant.deploymentId,
          sessionId: grant.sessionId,
          lifecycleEpoch: grant.lifecycleEpoch
        },
        sandboxId: grant.sandboxId,
        targetDeviceId: grant.targetDeviceId,
        lifecycleEpoch: grant.lifecycleEpoch,
        capabilities: grant.capabilities,
        grantedAt: grant.grantedAt,
        expiresAt: grant.expiresAt,
        revokedAt: grant.revokedAt
      })
    );
  }
  if (path === "/notifications/device" && identity.credential !== "device") {
    headers.set(NOTIFICATION_BEARER_HEADER, bearerFromRequest(request) ?? "");
  }
  if (roomKind) headers.set(ROOM_KIND_HEADER, roomKind);
  if (deviceHostAuthorization) {
    headers.set(DEVICE_HOST_AUTH_HEADER, deviceHostAuthorization);
  }
  return stub.fetch(new Request(url.toString(), { ...requestInit(request), headers }));
}, "forward");
var deviceParam = /* @__PURE__ */ __name((url) => {
  const device = url.searchParams.get("device") ?? "";
  return ID_RE4.test(device) ? `&device=${device}` : "";
}, "deviceParam");
var hasCapability2 = /* @__PURE__ */ __name((identity, capability) => identity.capabilities.includes(capability), "hasCapability");
var authenticate = /* @__PURE__ */ __name(async (request, env) => {
  const token = bearerFromRequest(request);
  if (!token) return void 0;
  return token.startsWith("cs1.") ? authenticateDeviceToken(env, token) : authenticateScaffold(env, request);
}, "authenticate");
var deviceCredentialAllows = /* @__PURE__ */ __name((identity, kind, id) => {
  if (identity.credential !== "device") return true;
  const scoped = identity;
  return kind === "session" ? scoped.sessionId === id : deviceGrantTargetsRoom(scoped.targetDeviceId, id);
}, "deviceCredentialAllows");
var sessionRoomKey = /* @__PURE__ */ __name((identity, sessionId, requestedDeploymentId) => {
  if (identity.credential !== "device") {
    return requestedDeploymentId ? scopedSessionRoomKey(identity.projectScope, requestedDeploymentId, sessionId) : `s3/${identity.projectScope}/${sessionId}`;
  }
  const scoped = identity;
  if (scoped.projectId !== identity.projectScope || scoped.sessionId !== sessionId || typeof scoped.deploymentId !== "string" || !ID_RE4.test(scoped.deploymentId) || requestedDeploymentId !== null && requestedDeploymentId !== void 0 && requestedDeploymentId !== scoped.deploymentId) {
    throw new Error("device_session_scope_invalid");
  }
  return scopedSessionRoomKey(scoped.projectId, scoped.deploymentId, sessionId);
}, "sessionRoomKey");
var peerSessionOwnerDevice = /* @__PURE__ */ __name(async (env, identity, sessionId, deploymentId) => {
  const room = deploymentId ? scopedSessionRoomKey(identity.projectScope, deploymentId, sessionId) : `s3/${identity.projectScope}/${sessionId}`;
  try {
    const stub = env.SESSION_ROOMS.get(env.SESSION_ROOMS.idFromName(room));
    const response = await stub.fetch(
      new Request("https://session.internal/authorize-owner", {
        headers: {
          [AUTH_USER_HEADER]: identity.userId,
          [AUTH_PROJECT_HEADER]: identity.projectScope,
          [SESSION_OWNER_AUTH_HEADER]: "verify"
        }
      })
    );
    if (!response.ok) return void 0;
    const value = await response.json().catch(() => null);
    return value?.ownsSession === true && typeof value.deviceId === "string" && ID_RE4.test(value.deviceId) ? value.deviceId : void 0;
  } catch {
    return void 0;
  }
}, "peerSessionOwnerDevice");
var index_default = {
  async fetch(request, env) {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);
    if (url.pathname === "/health") {
      return json5({ ok: true, auth: env.AUTH_MODE, environment: env.ENVIRONMENT });
    }
    if (!credentialTransportAllowed(url)) {
      return json5({ error: "secure_transport_required" }, 403);
    }
    const publicAuth = await handlePublicAuthRoute(request, env, url);
    if (publicAuth) return publicAuth;
    const identity = await authenticate(request, env);
    if (!identity) return json5({ error: "unauthenticated" }, 401);
    if (identity.projectScope !== env.SCAFFOLD_PROJECT_SCOPE) {
      return json5({ error: "forbidden" }, 403);
    }
    const authenticatedAuth = await handleAuthenticatedAuthRoute(request, env, url, identity);
    if (authenticatedAuth) return authenticatedAuth;
    if (parts[0] === "api" && parts[1] === "releases" && parts.length === 3 && RELEASE_FILE_RE.test(parts[2] ?? "")) {
      if (!hasCapability2(identity, "session.read") || !["GET", "HEAD"].includes(request.method)) {
        return json5({ error: "forbidden" }, 403);
      }
      return fetchReleaseObject(
        env,
        parts[2],
        request.method
      );
    }
    const deviceCredential = identity.credential === "device";
    if (url.pathname === "/notifications/device") {
      if (deviceCredential || !hasCapability2(identity, "session.read")) return json5({ error: "forbidden" }, 403);
      if (request.method !== "PUT" && request.method !== "DELETE") return json5({ error: "method_not_allowed" }, 405);
      return forward(env.SESSION_ROOMS, `ws4/${identity.projectScope}`, request, identity, "/notifications/device", "", "workspace");
    }
    const sessionId = canonicalSessionId(parts[1]);
    if (parts[0] === "session" && parts[2] === "directory-authority" && parts.length === 3) {
      if (request.method !== "GET") return json5({ error: "method_not_allowed" }, 405);
      if (!hasCapability2(identity, "session.control") || !sessionId || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      if (!sessionId || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(sessionId)) {
        return json5({ error: "invalid_session" }, 400);
      }
      const deploymentId = url.searchParams.get("deploymentId");
      if (deploymentId !== null && !ID_RE4.test(deploymentId)) return json5({ error: "invalid_deployment" }, 400);
      if (deviceCredential) {
        const grant = identity;
        if (!deploymentId || deploymentId !== grant.deploymentId) return json5({ error: "forbidden" }, 403);
      }
      try {
        const room = env.SESSION_ROOMS.get(env.SESSION_ROOMS.idFromName(sessionRoomKey(identity, sessionId, deploymentId)));
        const response = await room.fetch(new Request("https://session.internal/authorize-owner", {
          headers: {
            [AUTH_USER_HEADER]: identity.userId,
            [AUTH_PROJECT_HEADER]: identity.projectScope,
            [SESSION_OWNER_AUTH_HEADER]: "verify"
          }
        }));
        if (!response.ok) return json5({ error: "directory_authority_unavailable" }, 503);
        const result = await response.json();
        if (typeof result.ownsSession !== "boolean") return json5({ error: "directory_authority_unavailable" }, 503);
        return json5({ ownsSession: result.ownsSession, projectId: identity.projectScope, actorId: identity.email, subject: identity.userId });
      } catch {
        return json5({ error: "directory_authority_unavailable" }, 503);
      }
    }
    if (parts[0] === "session" && sessionId && ID_RE4.test(sessionId) && parts[2] === "ws") {
      if (!hasCapability2(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) {
        return json5({ error: "forbidden" }, 403);
      }
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json5({ error: "expected_websocket" }, 426);
      }
      return forward(
        env.SESSION_ROOMS,
        sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")),
        request,
        identity,
        "/ws",
        `?chatId=${sessionId}${deviceParam(url)}`
      );
    }
    if (parts[0] === "tail" && sessionId && ID_RE4.test(sessionId) && request.method === "GET") {
      if (!hasCapability2(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/tail");
    }
    if (parts[0] === "stats" && sessionId && ID_RE4.test(sessionId) && request.method === "GET") {
      if (!hasCapability2(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/stats");
    }
    if (parts[0] === "diff" && sessionId && ID_RE4.test(sessionId)) {
      const capability = request.method === "GET" ? "session.read" : "session.files";
      if (!hasCapability2(identity, capability) || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/diff");
    }
    if (parts[0] === "snapshot" && sessionId && ID_RE4.test(sessionId) && request.method === "GET") {
      if (!hasCapability2(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/snapshot");
    }
    if (parts[0] === "append" && sessionId && ID_RE4.test(sessionId) && request.method === "POST") {
      if (!hasCapability2(identity, "session.chat") || !deviceCredentialAllows(identity, "session", sessionId)) return json5({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/append");
    }
    if (parts[0] === "workspace" && parts[1] && ID_RE4.test(parts[1])) {
      if (parts[1] !== identity.projectScope || !hasCapability2(identity, "session.read")) {
        return json5({ error: "forbidden" }, 403);
      }
      const room = `ws4/${identity.projectScope}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return json5({ error: "expected_websocket" }, 426);
        return forward(env.SESSION_ROOMS, room, request, identity, "/ws", `?chatId=${encodeURIComponent(room)}${deviceParam(url)}`, "workspace");
      }
      if (parts[2] === "tail" && request.method === "GET") return forward(env.SESSION_ROOMS, room, request, identity, "/tail", "", "workspace");
      if (parts[2] === "stats" && request.method === "GET") return forward(env.SESSION_ROOMS, room, request, identity, "/stats", "", "workspace");
      if (parts[2] === "reset-log" && request.method === "POST") {
        if (deviceCredential || !hasCapability2(identity, "session.control")) return json5({ error: "forbidden" }, 403);
        return forward(env.SESSION_ROOMS, room, request, identity, "/reset-log", "", "workspace");
      }
    }
    if (parts[0] === "peer" && sessionId && parts[2] === "ws") {
      const deploymentId = url.searchParams.get("deploymentId") ?? void 0;
      if (!deviceCredential || request.method !== "GET" || request.headers.get("upgrade")?.toLowerCase() !== "websocket" || !hasCapability2(identity, "session.chat") || deploymentId !== void 0 && !ID_RE4.test(deploymentId)) {
        return json5({ error: "forbidden" }, 403);
      }
      const deviceId = await peerSessionOwnerDevice(env, identity, sessionId, deploymentId);
      if (!deviceId) return json5({ error: "target_session_not_found" }, 404);
      const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
      return forward(
        env.DEVICE_ROOMS,
        `d3/${identity.projectScope}/${deviceId}`,
        request,
        identity,
        "/ws",
        `?role=client&connId=${encodeURIComponent(connId)}&purpose=peer&peerSessionId=${encodeURIComponent(sessionId)}&targetDeviceId=${encodeURIComponent(deviceId)}`
      );
    }
    if (parts[0] === "device" && parts[1] && ID_RE4.test(parts[1])) {
      const deviceId = parts[1];
      const requestedRole = url.searchParams.get("role");
      const peerSessionId = canonicalSessionId(url.searchParams.get("peerSessionId") ?? void 0);
      const peerDeploymentId = url.searchParams.get("peerDeploymentId") ?? void 0;
      const peerPurpose = url.searchParams.get("purpose");
      const controlSessionId = peerPurpose === "control" ? canonicalSessionId(url.searchParams.get("controlSessionId") ?? void 0) : void 0;
      if (peerPurpose === "control" && (url.searchParams.has("controlDeploymentId") || url.searchParams.has("deploymentId"))) {
        return json5({ error: "scoped_control_not_supported" }, 403);
      }
      if (peerPurpose === "control" && (deviceCredential || !controlSessionId || requestedRole !== "client" || deviceId.startsWith(SANDBOX_DEVICE_PREFIX) || !hasCapability2(identity, "session.control") && !hasCapability2(identity, "session.read"))) return json5({ error: "forbidden" }, 403);
      const directPeerReply = peerPurpose === "peer-reply";
      const peerClient = parts[2] === "ws" && requestedRole === "client" && (deviceCredential && peerPurpose === "peer" || directPeerReply) && peerSessionId !== void 0 && (peerDeploymentId === void 0 || ID_RE4.test(peerDeploymentId));
      if (!deviceCredentialAllows(identity, "device", deviceId) && !(deviceCredential && peerClient)) {
        return json5({ error: "forbidden" }, 403);
      }
      const room = `d3/${identity.projectScope}/${deviceId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return json5({ error: "expected_websocket" }, 426);
        let hostAuthorization;
        if (requestedRole === "host") {
          if (deviceCredential) {
            hostAuthorization = "sandbox";
          } else if (!deviceId.startsWith(SANDBOX_DEVICE_PREFIX)) {
            hostAuthorization = "local";
          }
        }
        const role = authorizedDeviceSocketRole(
          requestedRole,
          deviceCredential,
          hostAuthorization,
          peerClient && !directPeerReply,
          directPeerReply
        );
        if (!role) return json5({ error: "forbidden" }, 403);
        if (role === "host" ? !hasCapability2(identity, "session.environment") : peerClient ? !hasCapability2(identity, "session.chat") : controlSessionId ? !hasCapability2(identity, "session.read") && !hasCapability2(identity, "session.control") : !hasCapability2(identity, "session.control") && !hasCapability2(identity, "session.environment")) {
          return json5({ error: "forbidden" }, 403);
        }
        if (peerClient && !directPeerReply) {
          const ownerDeviceId = await peerSessionOwnerDevice(
            env,
            identity,
            peerSessionId,
            peerDeploymentId
          );
          if (ownerDeviceId !== deviceId) return json5({ error: "forbidden" }, 403);
        }
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        const peer = peerClient ? `&purpose=${encodeURIComponent(peerPurpose)}&peerSessionId=${encodeURIComponent(peerSessionId)}&targetDeviceId=${encodeURIComponent(deviceId)}` : `&targetDeviceId=${encodeURIComponent(deviceId)}${controlSessionId ? `&controlSessionId=${encodeURIComponent(controlSessionId)}` : ""}`;
        return forward(
          env.DEVICE_ROOMS,
          room,
          request,
          identity,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}${peer}`,
          void 0,
          hostAuthorization
        );
      }
      if (deviceCredential) return json5({ error: "forbidden" }, 403);
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        if (!hasCapability2(identity, "session.environment")) return json5({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, `/sidecar/${parts[3]}`);
      }
      if (parts[2] === "status" && request.method === "GET") {
        if (!hasCapability2(identity, "session.read")) return json5({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, "/status");
      }
      if (parts[2] === "nudge" && request.method === "POST") {
        if (!hasCapability2(identity, "session.control")) return json5({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, "/nudge");
      }
    }
    if (parts[0] === "attachments" && parts[1] && SHA256_RE.test(parts[1])) {
      if (deviceCredential || !hasCapability2(identity, "session.files")) {
        return json5({ error: "forbidden" }, 403);
      }
      const key = `att/${identity.projectScope}/${parts[1]}`;
      if (request.method === "PUT") {
        const body = await request.arrayBuffer();
        if (body.byteLength > MAX_ATTACHMENT_BYTES) return json5({ error: "too_large" }, 413);
        const digest = await crypto.subtle.digest("SHA-256", body);
        const hex = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
        if (hex !== parts[1]) return json5({ error: "hash_mismatch" }, 400);
        await env.BLOBS.put(key, body, { httpMetadata: { contentType: request.headers.get("content-type") ?? "application/octet-stream" } });
        return json5({ ok: true, hash: hex, bytes: body.byteLength });
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object = request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
        if (!object) return json5({ error: "not_found" }, 404);
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set("etag", object.httpEtag);
        headers.set("cache-control", "private, max-age=31536000, immutable");
        const body = request.method === "GET" && "body" in object ? object.body : null;
        return new Response(body, { headers });
      }
    }
    return json5({ error: "not_found" }, 404);
  }
};
export {
  AuthGrant2 as AuthGrant,
  DeviceRoom,
  SessionRoom,
  index_default as default,
  sessionRoomKey
};
//# sourceMappingURL=index.js.map
