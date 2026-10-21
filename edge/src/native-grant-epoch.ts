import type { Env } from "./env";

/** Server-only DO protocol; never forwarded from public request headers. */
export const NATIVE_GRANT_ISSUER_HEADER = "x-crew-native-grant-issuer";
const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

export interface NativeGrantEpoch {
  ownerUserId: string;
  projectId: string;
  deploymentId: string;
  sessionId: string;
  sandboxId: string;
  targetDeviceId: string;
  lifecycleEpoch: number;
}

const validEpoch = (value: unknown): value is NativeGrantEpoch => {
  if (!value || typeof value !== "object") return false;
  const epoch = value as Partial<NativeGrantEpoch>;
  return typeof epoch.ownerUserId === "string" && epoch.ownerUserId.length > 0 &&
    typeof epoch.projectId === "string" && ID_RE.test(epoch.projectId) &&
    typeof epoch.deploymentId === "string" && ID_RE.test(epoch.deploymentId) &&
    typeof epoch.sessionId === "string" && ID_RE.test(epoch.sessionId) &&
    typeof epoch.sandboxId === "string" && ID_RE.test(epoch.sandboxId) &&
    typeof epoch.targetDeviceId === "string" && ID_RE.test(epoch.targetDeviceId) &&
    typeof epoch.lifecycleEpoch === "number" && Number.isSafeInteger(epoch.lifecycleEpoch) && epoch.lifecycleEpoch > 0;
};

// JSON tuple encoding cannot alias delimiter-bearing owners or other logical scopes.
// This prefix also cannot collide with the ordinary 32-hex grant object names.
const epochRoomName = (scope: NativeGrantEpoch): string => `native-epoch/${JSON.stringify([
  scope.ownerUserId, scope.projectId, scope.deploymentId, scope.sessionId, scope.sandboxId
])}`;

/** A check never creates authority: absent/corrupt/stale fences fail closed. */
export async function nativeGrantEpoch(
  namespace: DurableObjectNamespace,
  scope: NativeGrantEpoch,
  operation: "advance" | "check"
): Promise<boolean> {
  if (!validEpoch(scope)) return false;
  const room = namespace.get(namespace.idFromName(epochRoomName(scope)));
  const response = await room.fetch(`https://grant.internal/native-epoch/${operation}`, {
    method: "POST",
    headers: { [NATIVE_GRANT_ISSUER_HEADER]: "1" },
    body: JSON.stringify({
      ownerUserId: scope.ownerUserId, projectId: scope.projectId, deploymentId: scope.deploymentId,
      sessionId: scope.sessionId, sandboxId: scope.sandboxId,
      targetDeviceId: scope.targetDeviceId, lifecycleEpoch: scope.lifecycleEpoch
    })
  });
  if (response.status === 409) return false;
  if (!response.ok) throw new Error(`native grant epoch authority unavailable (${response.status})`);
  return true;
}

/** Runs under AuthGrant's blockConcurrencyWhile, including durable fence writes. */
export async function handleNativeGrantEpoch(
  ctx: DurableObjectState,
  env: Env,
  request: Request
): Promise<Response | undefined> {
  const path = new URL(request.url).pathname;
  if (path !== "/native-epoch/advance" && path !== "/native-epoch/check") return undefined;
  const namespace = env.NATIVE_GRANT_EPOCHS;
  if (!namespace || request.method !== "POST" || request.headers.get(NATIVE_GRANT_ISSUER_HEADER) !== "1") {
    return Response.json({ error: "forbidden" }, { status: 403 });
  }
  const incoming: unknown = await request.json().catch(() => undefined);
  if (!validEpoch(incoming) || incoming.projectId !== env.SCAFFOLD_PROJECT_SCOPE ||
      incoming.deploymentId !== env.SCAFFOLD_DEPLOYMENT_ID) {
    return Response.json({ error: "invalid_epoch_scope" }, { status: 403 });
  }
  const name = epochRoomName(incoming);
  // Ordinary grant IDs cannot invoke this protocol, even via a direct internal stub.
  if (namespace.idFromName(name).toString() !== ctx.id.toString()) {
    return Response.json({ error: "forbidden" }, { status: 403 });
  }
  const current = await ctx.storage.get<NativeGrantEpoch>("native-epoch");
  if (current && (!validEpoch(current) || epochRoomName(current) !== name)) {
    return Response.json({ error: "invalid_epoch_state" }, { status: 409 });
  }
  if (path === "/native-epoch/advance" && (!current || incoming.lifecycleEpoch > current.lifecycleEpoch)) {
    await ctx.storage.put("native-epoch", incoming);
    return Response.json({ ok: true });
  }
  if (!current || incoming.lifecycleEpoch !== current.lifecycleEpoch || incoming.targetDeviceId !== current.targetDeviceId) {
    return Response.json({ error: "stale_epoch" }, { status: 409 });
  }
  return Response.json({ ok: true });
}
