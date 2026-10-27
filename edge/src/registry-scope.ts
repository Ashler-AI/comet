import { applyOp, type FieldValue, type Op, type Row } from "./registry-core";
import type { SocketAuthority } from "./authority";

const object = (value: unknown): value is Record<string, FieldValue> =>
  value !== null && typeof value === "object" && !Array.isArray(value);
export const registryHost = (a?: SocketAuthority): boolean => !!a && a.credential === "device" &&
  a.writable && a.capabilities.includes("session.sync.write") && !!a.sessionId && !!a.deviceId &&
  !!a.deploymentId && !!a.sandboxId && Number.isSafeInteger(a.lifecycleEpoch) && a.lifecycleEpoch! >= 0 &&
  a.deviceId === `comet-scaffold-${a.sandboxId}-e${a.lifecycleEpoch}`;

export function crewMatches(value: unknown, a: SocketAuthority): boolean {
  return object(value) && value.ownerSubject === a.userId && value.sessionId === a.sessionId &&
    object(value.scope) && value.scope.projectId === a.projectId &&
    value.scope.deploymentId === a.deploymentId && value.scope.sessionId === a.sessionId;
}

/** The controller owns environment and routing. Never compare LWW clocks for authority. */
export function bindingMatches(chat: Row | undefined, a: SocketAuthority): boolean {
  if (!chat) return true;
  if (chat.deleted || chat.fields.deviceId !== a.deviceId) return false;
  const crew = chat.fields.crew;
  if (crew == null) return true;
  if (!crewMatches(crew, a) || !object(crew)) return false;
  if (crew.environment == null) return true;
  const environment = crew.environment;
  if (!object(environment) || environment.ownerPrincipal !== a.userId ||
      !object(environment.scope) || environment.scope.projectId !== a.projectId ||
      environment.scope.deploymentId !== a.deploymentId || environment.scope.sessionId !== a.sessionId ||
      !object(environment.source)) return false;
  const source = environment.source;
  return source.kind === "scaffold" && source.sandbox_id === a.sandboxId &&
    source.lifecycle_epoch === a.lifecycleEpoch && source.lifecycle !== "stopped";
}

export function visibleRow(row: Row, a: SocketAuthority): boolean {
  if (row.kind === "chats" || row.kind === "sessions") return row.id === a.sessionId &&
    (row.deleted || row.fields.deviceId === a.deviceId);
  if (row.kind === "devices") return row.id === a.deviceId;
  return row.kind === "spaces" && !row.deleted && row.fields.deviceId === a.deviceId;
}

// The actual RegistryDoc columns, not a generic row/table permission. Null is
// upstream's field-clear encoding; identity columns are checked separately.
const columns: Record<string, Record<string, string>> = {
  devices: { id: "string", name: "string", platform: "string", devboxProvider: "devboxProvider", devboxId: "string", lastSeenAt: "number", createdAt: "number", version: "string", cursorSdkVersion: "string", cursorSdkEngineVersion: "string", capabilities: "strings" },
  spaces: { id: "string", deviceId: "string", path: "string", name: "string", gitDetected: "boolean", gitCheckedAt: "number", checkoutId: "string", repositoryId: "string", createdAt: "number" },
  chats: { id: "string", deviceId: "string", title: "string", archived: "boolean", cwd: "string", branch: "string", checkoutId: "string", sourceContext: "object", config: "object", crew: "object", lastMessagePreview: "string", lastMessageAt: "number", createdAt: "number", harnessSessionId: "string", harnessSessionCwd: "string", spaceId: "string", parentChatId: "string", lastSeenAt: "number", roomGen: "number" },
  sessions: { chatId: "string", deviceId: "string", status: "status", lastCompletedTurn: "string", startedAt: "number", updatedAt: "number", runningSubagents: "number" },
};
function validValue(value: FieldValue, type?: string): boolean {
  if (!type) return false;
  if (value === null) return true;
  if (type === "devboxProvider") return value === "namespace" || value === "boat";
  if (type === "strings") return Array.isArray(value) && value.every(v => typeof v === "string");
  if (type === "object") return object(value);
  if (type === "status") return ["idle", "working", "awaitingInput", "errored"].includes(value as string);
  return typeof value === type && (type !== "number" || Number.isFinite(value));
}
/** The device is a single-session, epoch-bound Scaffold host, so its space may
 * arrive before its chat in upstream's separate pending batches. */
export function scopedOps(ops: Op[], a: SocketAuthority, load: (kind: string, id: string) => Row | undefined): Op[] | undefined {
  const normalized: Op[] = [];
  const projected = new Map<string, Row>();
  const get = (kind: string, id: string) => projected.get(`${kind}/${id}`) ?? load(kind, id);
  for (const op of ops) {
    if (!Object.hasOwn(columns, op.kind) || op.op === "delete" || !op.set) return undefined;
    if ((op.kind === "chats" || op.kind === "sessions") && op.id !== a.sessionId ||
        op.kind === "devices" && op.id !== a.deviceId) return undefined;
    const before = get(op.kind, op.id);
    if (before?.deleted || before?.fields.deviceId != null && before.fields.deviceId !== a.deviceId) return undefined;
    const set = { ...op.set };
    for (const [key, value] of Object.entries(set)) {
      if (!Object.hasOwn(columns[op.kind], key) || !validValue(value, columns[op.kind][key])) return undefined;
      if ((key === "id" && value !== op.id) || (key === "chatId" && value !== a.sessionId) ||
          (key === "deviceId" && value !== a.deviceId) ||
          (key === "parentChatId" && value !== null && value !== before?.fields.parentChatId)) return undefined;
      if (op.kind === "spaces" && key === "path" && (typeof value !== "string" ||
          !(value === "/workspace" || value.startsWith("/workspace/")) ||
          value.split("/").some(part => part === ".." || part === ".") || /[\\\u0000-\u001f]/.test(value))) return undefined;
    }
    if (op.kind === "chats") {
      if (set.crew != null && !crewMatches(set.crew, a)) return undefined;
      // Retain canonical metadata including its clock; a host can bootstrap the
      // identity, never publish/clear control-plane environment or route data.
      delete set.crew;
      if (op.op === "upsert" && set.config === null && before?.fields.config) delete set.config;
      if (!before?.fields.crew) set.crew = { ownerSubject: a.userId, sessionId: a.sessionId!, scope: {
        projectId: a.projectId, deploymentId: a.deploymentId!, sessionId: a.sessionId!,
      } };
    }
    const clocks = { ...op.clocks };
    for (const key of Object.keys(clocks)) if (!(key in set)) delete clocks[key];
    if (op.kind === "chats" && set.crew) clocks.crew = "0000000000000-000000-crew";
    const next = { ...op, set, clocks };
    const result = applyOp(before, next);
    if (result.row) projected.set(`${op.kind}/${op.id}`, result.row);
    normalized.push(next);
  }
  const chat = get("chats", a.sessionId!);
  for (const row of projected.values()) {
    if (!visibleRow(row, a)) return undefined;
    if ((row.kind === "chats" || row.kind === "sessions" || row.kind === "spaces") && row.fields.deviceId !== a.deviceId) return undefined;
    if (row.kind === "chats" && row.fields.id !== a.sessionId || row.kind === "sessions" && row.fields.chatId !== a.sessionId || row.kind === "devices" && row.fields.id !== a.deviceId) return undefined;
  }
  if (chat?.fields.spaceId != null) {
    const space = typeof chat.fields.spaceId === "string" ? get("spaces", chat.fields.spaceId) : undefined;
    // A controller may publish the chat reference before the host's space row.
    if (space && (space.deleted || space.fields.deviceId !== a.deviceId)) return undefined;
  }
  return normalized;
}
