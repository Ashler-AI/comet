/** Ashler Comet edge: Scaffold identity, scoped rooms, private GCS releases, and R2 attachments. */
import {
  authenticateScaffold,
  bearerFromRequest,
  credentialTransportAllowed,
  type Verified
} from "./auth";
import {
  authenticateDeviceToken,
  handleAuthenticatedAuthRoute,
  handlePublicAuthRoute
} from "./auth-routes";
import {
  AUTH_CAPABILITIES_HEADER,
  AUTH_GRANT_HEADER,
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  DEVICE_HOST_AUTH_HEADER,
  NOTIFICATION_BEARER_HEADER,
  ROOM_KIND_HEADER,
  SESSION_OWNER_AUTH_HEADER,
  stripTrustedAuthHeaders,
  type Env
} from "./env";
import { AuthGrant } from "./grant-authority";
import { scopedSessionRoomKey } from "./room-key";
import { fetchReleaseObject, type ReleaseFeedEnv } from "./release-feed";
import { canonicalSessionId, SessionRoom } from "./session-room";
import {
  authorizedDeviceSocketRole,
  deviceGrantTargetsRoom,
  DeviceRoom,
  type DeviceHostAuthorization
} from "./device-room";

export { SessionRoom, DeviceRoom, AuthGrant };

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const SHA256_RE = /^[a-f0-9]{64}$/;
const MAX_ATTACHMENT_BYTES = 32 * 1024 * 1024;
const SANDBOX_DEVICE_PREFIX = "comet-scaffold-";
const RELEASE_FILE_RE = /^[A-Za-z0-9._-]{1,200}$/;

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const requestInit = (request: Request): RequestInit => ({
  method: request.method,
  body: request.body
});

const forward = (
  ns: DurableObjectNamespace,
  name: string,
  request: Request,
  identity: Verified,
  path: string,
  search = "",
  roomKind?: "workspace",
  deviceHostAuthorization?: DeviceHostAuthorization
): Promise<Response> => {
  const stub = ns.get(ns.idFromName(name));
  const url = new URL(request.url);
  url.pathname = path;
  url.search = search;
  const headers = new Headers(request.headers);
  stripTrustedAuthHeaders(headers);
  headers.delete("authorization");
  headers.set(AUTH_USER_HEADER, identity.userId);
  headers.set(AUTH_PROJECT_HEADER, identity.projectScope);
  headers.set(AUTH_CAPABILITIES_HEADER, identity.capabilities.join(" "));
  if (identity.credential === "device") {
    const grant = identity as Verified & {
      grantId: string;
      projectId: string;
      deploymentId: string;
      sandboxId: string;
      targetDeviceId: string;
      sessionId: string;
      lifecycleEpoch: number;
      grantedAt: number;
      expiresAt: number;
      revokedAt: null;
    };
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
};

const deviceParam = (url: URL): string => {
  const device = url.searchParams.get("device") ?? "";
  return ID_RE.test(device) ? `&device=${device}` : "";
};

const hasCapability = (identity: Verified, capability: string): boolean =>
  identity.capabilities.includes(capability);

const authenticate = async (request: Request, env: Env): Promise<Verified | undefined> => {
  const token = bearerFromRequest(request);
  if (!token) return undefined;
  return token.startsWith("cs1.")
    ? authenticateDeviceToken(env, token)
    : authenticateScaffold(env, request);
};

const deviceCredentialAllows = (
  identity: Verified,
  kind: "session" | "device",
  id: string
): boolean => {
  if (identity.credential !== "device") return true;
  const scoped = identity as Verified & { targetDeviceId?: string; sessionId?: string };
  return kind === "session" ? scoped.sessionId === id : deviceGrantTargetsRoom(scoped.targetDeviceId, id);
};

export const sessionRoomKey = (
  identity: Verified,
  sessionId: string,
  requestedDeploymentId?: string | null
): string => {
  if (identity.credential !== "device") {
    return requestedDeploymentId
      ? scopedSessionRoomKey(identity.projectScope, requestedDeploymentId, sessionId)
      : `s3/${identity.projectScope}/${sessionId}`;
  }
  const scoped = identity as Verified & { projectId?: string; deploymentId?: string; sessionId?: string };
  if (
    scoped.projectId !== identity.projectScope ||
    scoped.sessionId !== sessionId ||
    typeof scoped.deploymentId !== "string" ||
    !ID_RE.test(scoped.deploymentId) ||
    (requestedDeploymentId !== null &&
      requestedDeploymentId !== undefined &&
      requestedDeploymentId !== scoped.deploymentId)
  ) {
    throw new Error("device_session_scope_invalid");
  }
  return scopedSessionRoomKey(scoped.projectId, scoped.deploymentId, sessionId);
};

const peerSessionOwnerDevice = async (
  env: Env,
  identity: Verified,
  sessionId: string,
  deploymentId?: string
): Promise<string | undefined> => {
  const room = deploymentId
    ? scopedSessionRoomKey(identity.projectScope, deploymentId, sessionId)
    : `s3/${identity.projectScope}/${sessionId}`;
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
    if (!response.ok) return undefined;
    const value = (await response.json().catch(() => null)) as {
      ownsSession?: unknown;
      deviceId?: unknown;
    } | null;
    return value?.ownsSession === true && typeof value.deviceId === "string" && ID_RE.test(value.deviceId)
      ? value.deviceId
      : undefined;
  } catch {
    return undefined;
  }
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (url.pathname === "/health") {
      return json({ ok: true, auth: env.AUTH_MODE, environment: env.ENVIRONMENT });
    }
    if (!credentialTransportAllowed(url)) {
      return json({ error: "secure_transport_required" }, 403);
    }
    const publicAuth = await handlePublicAuthRoute(request, env, url);
    if (publicAuth) return publicAuth;

    const identity = await authenticate(request, env);
    if (!identity) return json({ error: "unauthenticated" }, 401);
    if (identity.projectScope !== env.SCAFFOLD_PROJECT_SCOPE) {
      return json({ error: "forbidden" }, 403);
    }

    const authenticatedAuth = await handleAuthenticatedAuthRoute(request, env, url, identity);
    if (authenticatedAuth) return authenticatedAuth;
    if (
      parts[0] === "api" &&
      parts[1] === "releases" &&
      parts.length === 3 &&
      RELEASE_FILE_RE.test(parts[2] ?? "")
    ) {
      if (
        !hasCapability(identity, "session.read") ||
        !["GET", "HEAD"].includes(request.method)
      ) {
        return json({ error: "forbidden" }, 403);
      }
      return fetchReleaseObject(
        env as Env & ReleaseFeedEnv,
        parts[2],
        request.method as "GET" | "HEAD"
      );
    }


    // Sandbox credentials share their project workspace but remain scoped to
    // one session and device; other routes must opt in below.
    const deviceCredential = identity.credential === "device";

    if (url.pathname === "/notifications/device") {
      if (deviceCredential || !hasCapability(identity, "session.read")) return json({ error: "forbidden" }, 403);
      if (request.method !== "PUT" && request.method !== "DELETE") return json({ error: "method_not_allowed" }, 405);
      return forward(env.SESSION_ROOMS, `ws4/${identity.projectScope}`, request, identity, "/notifications/device", "", "workspace");
    }

    const sessionId = canonicalSessionId(parts[1]);
    if (parts[0] === "session" && parts[2] === "directory-authority" && parts.length === 3) {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      if (!hasCapability(identity, "session.control") || !sessionId || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      if (!sessionId || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(sessionId)) {
        return json({ error: "invalid_session" }, 400);
      }
      const deploymentId = url.searchParams.get("deploymentId");
      if (deploymentId !== null && !ID_RE.test(deploymentId)) return json({ error: "invalid_deployment" }, 400);
      if (deviceCredential) {
        const grant = identity as Verified & { deploymentId?: string };
        if (!deploymentId || deploymentId !== grant.deploymentId) return json({ error: "forbidden" }, 403);
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
        if (!response.ok) return json({ error: "directory_authority_unavailable" }, 503);
        const result = await response.json() as { ownsSession?: unknown };
        if (typeof result.ownsSession !== "boolean") return json({ error: "directory_authority_unavailable" }, 503);
        return json({ ownsSession: result.ownsSession, projectId: identity.projectScope, actorId: identity.email, subject: identity.userId });
      } catch {
        return json({ error: "directory_authority_unavailable" }, 503);
      }
    }
    if (parts[0] === "session" && sessionId && ID_RE.test(sessionId) && parts[2] === "ws") {
      if (!hasCapability(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) {
        return json({ error: "forbidden" }, 403);
      }
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json({ error: "expected_websocket" }, 426);
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
    if (parts[0] === "tail" && sessionId && ID_RE.test(sessionId) && request.method === "GET") {
      if (!hasCapability(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/tail");
    }
    if (parts[0] === "stats" && sessionId && ID_RE.test(sessionId) && request.method === "GET") {
      if (!hasCapability(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/stats");
    }
    if (parts[0] === "diff" && sessionId && ID_RE.test(sessionId)) {
      const capability = request.method === "GET" ? "session.read" : "session.files";
      if (!hasCapability(identity, capability) || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/diff");
    }
    if (parts[0] === "snapshot" && sessionId && ID_RE.test(sessionId) && request.method === "GET") {
      if (!hasCapability(identity, "session.read") || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/snapshot");
    }
    if (parts[0] === "append" && sessionId && ID_RE.test(sessionId) && request.method === "POST") {
      if (!hasCapability(identity, "session.chat") || !deviceCredentialAllows(identity, "session", sessionId)) return json({ error: "forbidden" }, 403);
      return forward(env.SESSION_ROOMS, sessionRoomKey(identity, sessionId, url.searchParams.get("deploymentId")), request, identity, "/append");
    }

    if (parts[0] === "workspace" && parts[1] && ID_RE.test(parts[1])) {
      if (
        parts[1] !== identity.projectScope ||
        !hasCapability(identity, "session.read")
      ) {
        return json({ error: "forbidden" }, 403);
      }
      const room = `ws4/${identity.projectScope}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return json({ error: "expected_websocket" }, 426);
        return forward(env.SESSION_ROOMS, room, request, identity, "/ws", `?chatId=${encodeURIComponent(room)}${deviceParam(url)}`, "workspace");
      }
      if (parts[2] === "tail" && request.method === "GET") return forward(env.SESSION_ROOMS, room, request, identity, "/tail", "", "workspace");
      if (parts[2] === "stats" && request.method === "GET") return forward(env.SESSION_ROOMS, room, request, identity, "/stats", "", "workspace");
      if (parts[2] === "reset-log" && request.method === "POST") {
        if (deviceCredential || !hasCapability(identity, "session.control")) return json({ error: "forbidden" }, 403);
        return forward(env.SESSION_ROOMS, room, request, identity, "/reset-log", "", "workspace");
      }
    }

    if (parts[0] === "peer" && sessionId && parts[2] === "ws") {
      const deploymentId = url.searchParams.get("deploymentId") ?? undefined;
      if (
        !deviceCredential ||
        request.method !== "GET" ||
        request.headers.get("upgrade")?.toLowerCase() !== "websocket" ||
        !hasCapability(identity, "session.chat") ||
        (deploymentId !== undefined && !ID_RE.test(deploymentId))
      ) {
        return json({ error: "forbidden" }, 403);
      }
      const deviceId = await peerSessionOwnerDevice(env, identity, sessionId, deploymentId)
        ?? (deploymentId ? await peerSessionOwnerDevice(env, identity, sessionId) : undefined);
      if (!deviceId) return json({ error: "target_session_not_found" }, 404);
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

    if (parts[0] === "device" && parts[1] && ID_RE.test(parts[1])) {
      const deviceId = parts[1];
      const requestedRole = url.searchParams.get("role");
      const peerSessionId = canonicalSessionId(url.searchParams.get("peerSessionId") ?? undefined);
      const peerDeploymentId = url.searchParams.get("peerDeploymentId") ?? undefined;
      const peerPurpose = url.searchParams.get("purpose");
      const controlSessionId = peerPurpose === "control"
        ? canonicalSessionId(url.searchParams.get("controlSessionId") ?? undefined)
        : undefined;
      if (peerPurpose === "control" && (
        deviceCredential || !controlSessionId || requestedRole !== "client" ||
        deviceId.startsWith(SANDBOX_DEVICE_PREFIX) || !hasCapability(identity, "session.control")
      )) return json({ error: "forbidden" }, 403);
      const directPeerReply = peerPurpose === "peer-reply";
      const peerClient =
        parts[2] === "ws" &&
        requestedRole === "client" &&
        ((deviceCredential && peerPurpose === "peer") || directPeerReply) &&
        peerSessionId !== undefined &&
        (peerDeploymentId === undefined || ID_RE.test(peerDeploymentId));
      if (!deviceCredentialAllows(identity, "device", deviceId) && !(deviceCredential && peerClient)) {
        return json({ error: "forbidden" }, 403);
      }
      const room = `d3/${identity.projectScope}/${deviceId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") return json({ error: "expected_websocket" }, 426);
        let hostAuthorization: DeviceHostAuthorization | undefined;
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
        if (!role) return json({ error: "forbidden" }, 403);
        if (
          role === "host"
            ? !hasCapability(identity, "session.environment")
            : peerClient
              ? !hasCapability(identity, "session.chat")
              : !hasCapability(identity, "session.control") &&
                !hasCapability(identity, "session.environment")
        ) {
          return json({ error: "forbidden" }, 403);
        }
        if (peerClient && !directPeerReply) {
          const ownerDeviceId = await peerSessionOwnerDevice(
            env,
            identity,
            peerSessionId!,
            peerDeploymentId
          );
          if (ownerDeviceId !== deviceId) return json({ error: "forbidden" }, 403);
        }
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        const peer = peerClient
          ? `&purpose=${encodeURIComponent(peerPurpose!)}&peerSessionId=${encodeURIComponent(peerSessionId!)}&targetDeviceId=${encodeURIComponent(deviceId)}`
          : `&targetDeviceId=${encodeURIComponent(deviceId)}${controlSessionId ? `&controlSessionId=${encodeURIComponent(controlSessionId)}` : ""}`;
        return forward(
          env.DEVICE_ROOMS,
          room,
          request,
          identity,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}${peer}`,
          undefined,
          hostAuthorization
        );
      }
      if (deviceCredential) return json({ error: "forbidden" }, 403);
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        if (!hasCapability(identity, "session.environment")) return json({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, `/sidecar/${parts[3]}`);
      }
      if (parts[2] === "status" && request.method === "GET") {
        if (!hasCapability(identity, "session.read")) return json({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, "/status");
      }
      if (parts[2] === "nudge" && request.method === "POST") {
        if (!hasCapability(identity, "session.control")) return json({ error: "forbidden" }, 403);
        return forward(env.DEVICE_ROOMS, room, request, identity, "/nudge");
      }
    }

    if (parts[0] === "attachments" && parts[1] && SHA256_RE.test(parts[1])) {
      if (deviceCredential || !hasCapability(identity, "session.files")) {
        return json({ error: "forbidden" }, 403);
      }
      const key = `att/${identity.projectScope}/${parts[1]}`;
      if (request.method === "PUT") {
        const body = await request.arrayBuffer();
        if (body.byteLength > MAX_ATTACHMENT_BYTES) return json({ error: "too_large" }, 413);
        const digest = await crypto.subtle.digest("SHA-256", body);
        const hex = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
        if (hex !== parts[1]) return json({ error: "hash_mismatch" }, 400);
        await env.BLOBS.put(key, body, { httpMetadata: { contentType: request.headers.get("content-type") ?? "application/octet-stream" } });
        return json({ ok: true, hash: hex, bytes: body.byteLength });
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object = request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
        if (!object) return json({ error: "not_found" }, 404);
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set("etag", object.httpEtag);
        headers.set("cache-control", "private, max-age=31536000, immutable");
        const body = request.method === "GET" && "body" in object ? (object as R2ObjectBody).body : null;
        return new Response(body, { headers });
      }
    }

    return json({ error: "not_found" }, 404);
  }
} satisfies ExportedHandler<Env>;
