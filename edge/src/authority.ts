import { authenticateResult, bearerFromRequest, type Verified } from "./auth";
import type { Env } from "./env";
import { registryHost } from "./registry-scope";
import type { PeerAuthority } from "./peer-route";

export const AUTHORITY_HEADER = "x-crew-authority";
export interface SocketAuthority {
  token: string; userId: string; writable: boolean; sessionId?: string;
  capabilities: readonly string[];
  projectId: string;
  deploymentId?: string;
  deviceId?: string;
  grantId?: string;
  expiresAt?: number;
  lifecycleEpoch?: number;
  sandboxId?: string;
  credential: Verified["credential"];
}

/** Opaque sync writes require an explicit whole-document capability, never a semantic permission. */
export const canWriteDocument = (identity: Verified): boolean =>
  identity.credential !== "device" || identity.capabilities.includes("session.sync.write");

export const authorizeRoute = (request: Request, identity: Verified): boolean => {
  const url = new URL(request.url);
  for (const [key, expected] of [["projectId", identity.projectScope], ["deploymentId", identity.deploymentId]] as const) {
    if (url.searchParams.has(key) && url.searchParams.get(key) !== expected) return false;
  }
  if (identity.credential !== "device") return true;
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts[0] === "registry" && parts[1] === identity.projectScope && parts.length === 3) {
    const host = registryHost({
      ...identity, token: "", projectId: identity.projectScope, deviceId: identity.targetDeviceId,
      writable: canWriteDocument(identity),
    });
    return host && ((request.method === "GET" && ["ws", "rows"].includes(parts[2])) ||
      (request.method === "POST" && parts[2] === "push")) &&
      (!url.searchParams.has("device") || url.searchParams.get("device") === identity.targetDeviceId);
  }
  if (parts[0] === "session" && parts[1] === identity.sessionId &&
      parts[2] === "directory-authority" && parts.length === 3) {
    return request.method === "GET" && identity.capabilities.includes("session.control") &&
      !!identity.deploymentId && url.searchParams.get("deploymentId") === identity.deploymentId;
  }
  // Authenticated application artifacts contain no personal registry/session data.
  if (parts[0] === "releases" || (parts[0] === "api" && parts[1] === "releases")) {
    return ["GET", "HEAD"].includes(request.method) && identity.capabilities.includes("session.read");
  }
  if (parts[0] === "chat2" && parts[1] === identity.sessionId) {
    if (!identity.capabilities.includes("session.read")) return false;
    if (parts[2] === "reset") return false;
    return request.method === "GET" || canWriteDocument(identity);
  }
  if (parts[0] === "blob" && parts[1] === identity.sessionId) {
    return identity.capabilities.includes("session.read") &&
      (["GET", "HEAD"].includes(request.method) || (request.method === "PUT" && canWriteDocument(identity)));
  }
  if (parts[0] === "device" && parts[1] === identity.targetDeviceId) {
    return (parts[2] === "status" && request.method === "GET") ||
      (parts[2] === "ws" && identity.capabilities.includes("session.read") &&
        (url.searchParams.get("role") !== "host" || canWriteDocument(identity)));
  }
  return false;
};

export const trustedRequest = (request: Request, identity: Verified): Request => {
  const headers = new Headers(request.headers);
  for (const key of [...headers.keys()]) {
    if (key.startsWith("x-crew-") || key.startsWith("x-comet-") || key.startsWith("x-zeron-")) headers.delete(key);
  }
  const authority: SocketAuthority = {
    token: bearerFromRequest(request) ?? "", userId: identity.userId,
    writable: canWriteDocument(identity), sessionId: identity.credential === "device" ? identity.sessionId : undefined,
    capabilities: identity.capabilities,
    projectId: identity.projectScope,
    deploymentId: identity.deploymentId,
    deviceId: identity.targetDeviceId,
    grantId: identity.grantId,
    expiresAt: identity.expiresAt,
    lifecycleEpoch: identity.lifecycleEpoch,
    sandboxId: identity.sandboxId,
    credential: identity.credential
  };
  headers.set(AUTHORITY_HEADER, JSON.stringify(authority));
  headers.delete("authorization");
  const url = new URL(request.url);
  url.searchParams.delete("token");
  return new Request(url, { method: request.method, headers, body: request.body });
};

export const requestAuthority = (request: Request): SocketAuthority | undefined => {
  const value = request.headers.get(AUTHORITY_HEADER);
  return value ? JSON.parse(value) as SocketAuthority : undefined;
};

/** Revalidate before receiving AND delivering data: silent revoked readers receive no further rows. */
export const socketAuthorized = async (env: Env, ws: WebSocket): Promise<boolean> => {
  const state = ws.deserializeAttachment() as { authority?: SocketAuthority } | null;
  if (!state?.authority) {
    // Direct DO fixtures have no Worker authority; deployed rooms must fail closed.
    if (env.AUTH_MODE === "dev" && env.ENVIRONMENT === "local") return true;
    ws.close(4401, "authentication required"); return false;
  }
  const result = await authenticateResult(env, new Request("https://authority.internal/", {
    headers: { authorization: `Bearer ${state.authority.token}` }
  }));
  if (result.status === "authenticated") {
    const fresh = result.identity, bound = state.authority;
    if (fresh.userId === bound.userId && fresh.projectScope === bound.projectId &&
        fresh.deploymentId === bound.deploymentId && fresh.sessionId === bound.sessionId &&
        fresh.targetDeviceId === bound.deviceId && fresh.grantId === bound.grantId &&
        fresh.expiresAt === bound.expiresAt && fresh.lifecycleEpoch === bound.lifecycleEpoch &&
        fresh.sandboxId === bound.sandboxId && fresh.credential === bound.credential &&
        canWriteDocument(fresh) === bound.writable &&
        fresh.capabilities.length === bound.capabilities.length &&
        fresh.capabilities.every(capability => bound.capabilities.includes(capability))) return true;
  }
  ws.close(result.status === "unavailable" ? 1013 : 4401,
    result.status === "unavailable" ? "authority unavailable" : "authority expired or revoked");
  return false;
};

type SocketData = string | ArrayBuffer | Uint8Array;
const sends = new WeakMap<WebSocket, SocketData[]>();
/** A backfill batch shares one fresh check, then sends synchronously in wire order.
 * No positive authority cache survives the batch: revoked silent readers cannot receive later updates. */
export const sendAuthorized = (env: Env, ctx: DurableObjectState, ws: WebSocket, data: SocketData): void => {
  const pending = sends.get(ws);
  if (pending) { pending.push(data); return; }
  const batch = [data];
  sends.set(ws, batch);
  ctx.waitUntil((async () => {
    try {
      if (await socketAuthorized(env, ws)) {
        for (const frame of batch) ws.send(frame);
      }
    } catch {
      try { ws.close(1013, "authority unavailable"); } catch { /* already closed */ }
    } finally {
      sends.delete(ws);
    }
  })());
};

/** Restricted peers use typed host commands; never raw terminal/filesystem RPC or opaque row mutation. */
export const scopedRpcAllowed = (kind: string, payload: Uint8Array, authority?: SocketAuthority): boolean => {
  if (kind === "echo") return payload.length === 0 && (!authority?.sessionId || authority.capabilities.includes("session.read"));
  if (!authority?.sessionId) return true;
  if (kind !== "rpc") return false;
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    // Cancellation is scoped by the relay's connection id and never dispatches a method.
    if (value?.cancel === true && !value.method) return true;
    const params = value?.params;
    if (value?.method === "LocalDevice" && (!params || Object.keys(params).length === 0)) {
      return authority.capabilities.includes("session.read");
    }
    if (!params || params.chatId !== authority.sessionId) return false;
    if (["WatchDocMessages", "WatchQueue", "FocusChat", "ListCrewAnnotations"].includes(value.method)) {
      return authority.capabilities.includes("session.read");
    }
    if (value.method === "ApplyCrewAnnotation") return authority.capabilities.includes("session.annotate");
    if (value.method !== "QueueCommand") return false;
    const command = params.command;
    if (!command || !["run", "steer", "interrupt", "respondInput"].includes(command.kind)) return false;
    return authority.capabilities.includes(command.kind === "interrupt" ? "session.control" : "session.chat");
  } catch {
    return false;
  }
};

/** Only DeviceRoom may supply this nonsecret envelope; caller claims are always overwritten. */
export const stampRpcAuthority = (payload: Uint8Array, authority?: SocketAuthority, peer?: PeerAuthority): Uint8Array | undefined => {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
    if (value.cancel !== true && typeof value.method !== "string") return undefined;
    const params = value.params ?? {};
    if (typeof params !== "object" || Array.isArray(params)) return undefined;
    delete params.__crewAuthority;
    if (authority) {
      params.__crewAuthority = {
        subject: authority.userId, projectId: authority.projectId,
        deploymentId: authority.deploymentId, sessionId: authority.sessionId,
        deviceId: authority.deviceId, grantId: authority.grantId,
        expiresAt: authority.expiresAt, lifecycleEpoch: authority.lifecycleEpoch,
        capabilities: authority.capabilities, ...(peer ? { peer } : {})
      };
    }
    return new TextEncoder().encode(JSON.stringify({ ...value, params }));
  } catch {
    return undefined;
  }
};
