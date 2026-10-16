import type { Verified } from "./auth";
import { AUTHORITY_HEADER, requestAuthority, trustedRequest, type SocketAuthority } from "./authority";
import { AUTH_USER_HEADER, type Env } from "./env";

export interface PeerAuthority {
  sourceSessionId: string;
  sourceDeviceId: string;
  targetSessionId: string;
  targetDeviceId: string;
}

/** Internal typed lookup; never returns the personal registry to a scoped caller. */
export async function peerSessionDevice(env: Env, authority: SocketAuthority, sessionId: string): Promise<string | undefined> {
  const room = env.REGISTRY_ROOMS.get(env.REGISTRY_ROOMS.idFromName(`reg1/${authority.projectId}/${authority.userId}`));
  const response = await room.fetch(new Request(`https://registry.internal/peer-target?sessionId=${encodeURIComponent(sessionId)}`, {
    headers: { [AUTH_USER_HEADER]: authority.userId, [AUTHORITY_HEADER]: JSON.stringify(authority) },
  }));
  if (response.status === 404 || response.status === 403) return undefined;
  if (!response.ok) throw new Error("Peer authority unavailable");
  const body = await response.json() as { deviceId?: unknown };
  return typeof body.deviceId === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(body.deviceId) ? body.deviceId : undefined;
}

export async function peerRoute(request: Request, env: Env, identity: Verified): Promise<Response | undefined> {
  const url = new URL(request.url);
  const session = /^\/session\/([A-Za-z0-9_-]{1,128})\/peer\/ws$/.exec(url.pathname);
  const direct = /^\/device\/([A-Za-z0-9_-]{1,128})\/ws$/.exec(url.pathname);
  if (!session && !(direct && url.searchParams.get("purpose") === "peer-reply")) return;
  const targetSessionId = session?.[1] ?? url.searchParams.get("peerSessionId");
  const deploymentId = url.searchParams.get(session ? "deploymentId" : "peerDeploymentId");
  if (request.method !== "GET" || request.headers.get("upgrade")?.toLowerCase() !== "websocket" ||
      !identity.capabilities.includes("session.chat") || !targetSessionId || !/^[A-Za-z0-9_-]{1,128}$/.test(targetSessionId) ||
      !identity.deploymentId || deploymentId !== identity.deploymentId ||
      (url.searchParams.has("projectId") && url.searchParams.get("projectId") !== identity.projectScope) ||
      (url.searchParams.has("deploymentId") && url.searchParams.get("deploymentId") !== identity.deploymentId) ||
      (direct && url.searchParams.get("role") !== "client")) return Response.json({ error: "forbidden" }, { status: 403 });
  request = trustedRequest(request, identity);
  const authority = requestAuthority(request)!;
  try {
    const deviceId = await peerSessionDevice(env, authority, targetSessionId);
    if (!deviceId || (direct && direct[1] !== deviceId)) return Response.json({ error: "target_session_not_found" }, { status: 404 });
    const target = new URL(request.url);
    target.pathname = "/ws";
    target.search = "";
    target.searchParams.set("role", "client");
    target.searchParams.set("purpose", direct ? "peer-reply" : "peer");
    target.searchParams.set("peerSessionId", targetSessionId);
    target.searchParams.set("targetDeviceId", deviceId);
    target.searchParams.set("connId", crypto.randomUUID());
    const headers = new Headers(request.headers);
    headers.set(AUTH_USER_HEADER, identity.userId);
    const room = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(`d2/${deviceId}`));
    return room.fetch(new Request(target, { headers }));
  } catch {
    return Response.json({ error: "peer_authority_unavailable" }, { status: 503 });
  }
}

/** Only this typed message may cross a session-bound relay. */
export function peerMessageAuthority(payload: Uint8Array, authority: SocketAuthority, target: Pick<PeerAuthority, "targetSessionId" | "targetDeviceId">, reply: boolean): PeerAuthority | undefined {
  try {
    const value = JSON.parse(new TextDecoder().decode(payload));
    const command = value?.params?.command;
    if (value.method !== "DeliverPeerMessage" || value.params?.chatId !== target.targetSessionId ||
        typeof value.params.commandId !== "string" || !value.params.commandId.length || value.params.commandId.length > 256 ||
        command?.kind !== "peerMessage" || typeof command.sourceChatId !== "string" || !command.sourceChatId.length ||
        typeof command.sourceDeviceId !== "string" || !command.sourceDeviceId.length ||
        (authority.sessionId && (command.sourceChatId !== authority.sessionId || command.sourceDeviceId !== authority.deviceId || command.sourceDeploymentId !== authority.deploymentId)) ||
        (command.sourceDeploymentId !== undefined && command.sourceDeploymentId !== authority.deploymentId) ||
        typeof command.threadId !== "string" || !command.threadId.length || command.threadId.length > 256 ||
        !(command.replyTo === null || (typeof command.replyTo === "string" && command.replyTo.length > 0 && command.replyTo.length <= 256)) ||
        !Number.isSafeInteger(command.hopCount) || command.hopCount < (reply ? 1 : 0) || command.hopCount > 8 ||
        (reply && typeof command.replyTo !== "string") ||
        typeof command.text !== "string" || !command.text.trim() || command.text.length > 256 * 1024) return;
    return { sourceSessionId: command.sourceChatId, sourceDeviceId: command.sourceDeviceId, ...target };
  } catch { return; }
}
