import { readFileSync } from "node:fs";
import { expect, it } from "vitest";
import { authorizeRoute, type SocketAuthority } from "./authority";
import type { Verified } from "./auth";
import type { FieldValue, Op } from "./registry-core";
import { registryColumns, scopedOps } from "./registry-scope";

it("only opens typed registry transports for an epoch-bound Scaffold writer", () => {
  const host: Verified = {
    credential: "device", userId: "owner", email: "owner", orgId: "canary", projectScope: "canary",
    deploymentId: "canary", sessionId: "one", sandboxId: "box", lifecycleEpoch: 2,
    targetDeviceId: "comet-scaffold-box-e2", capabilities: ["session.read", "session.sync.write"],
  };
  for (const [path, method] of [["rows", "GET"], ["ws", "GET"], ["push", "POST"]]) {
    const request = new Request(`https://edge/registry/canary/${path}`, { method });
    expect(authorizeRoute(request, host)).toBe(true);
    for (const changed of [
      { capabilities: ["session.read"] }, { sandboxId: undefined }, { lifecycleEpoch: undefined },
      { targetDeviceId: "desktop" }, { lifecycleEpoch: 3 }, { sessionId: undefined },
    ]) expect(authorizeRoute(request, { ...host, ...changed })).toBe(false);
  }
  for (const [path, method] of [["stats", "GET"], ["reset", "POST"], ["push-target", "POST"], ["device-presence", "POST"], ["peer-target", "GET"], ["rows/extra", "GET"]]) {
    expect(authorizeRoute(new Request(`https://edge/registry/canary/${path}`, { method }), host)).toBe(false);
  }
  expect(authorizeRoute(new Request("https://edge/registry/canary/ws?device=desktop"), host)).toBe(false);
  expect(authorizeRoute(new Request("https://edge/registry/other/rows"), host)).toBe(false);
});

// Failure mode (2026-11-13): the Edge's typed host allow-list drifted behind the
// native registry, so every Scaffold host batch carrying a newer column was
// rejected whole ("outside typed host registry scope") and never reached peers.
const host: SocketAuthority = {
  token: "t", userId: "owner@example.com", writable: true, credential: "device", projectId: "ashler",
  deploymentId: "ashler", sessionId: "chat-1", deviceId: "comet-scaffold-box-e2", sandboxId: "box", lifecycleEpoch: 2,
  capabilities: ["session.read", "session.sync.write"],
};
const at = "2026-11-13T00:00:00Z";
const op = (kind: string, id: string, set: Record<string, FieldValue>): Op =>
  ({ kind, id, op: "upsert", hlc: "1794528000000-000001-comet-scaffold-box-e2", set });

it("admits a Scaffold host's batch exactly as the native engine writes it", () => {
  const ops = [
    op("spaces", "space-1", { id: "space-1", deviceId: host.deviceId!, path: "/workspace/ashler-platform", name: "ashler-platform",
      gitDetected: true, gitCheckedAt: 1, checkoutId: "c", repositoryId: "r", createdAt: 1, githubRepo: "Ashler-AI/ashler-platform" }),
    op("chats", "chat-1", { id: "chat-1", deviceId: host.deviceId!, cwd: "/workspace/ashler-platform", roomGen: 2, spaceId: "space-1",
      harnessSessionId: "h", harnessSessionCwd: "/workspace/ashler-platform", harnessSessionHarness: "omp" }),
    op("sessions", "chat-1", { chatId: "chat-1", deviceId: host.deviceId!, status: "idle", lastCompletedTurn: "turn-1",
      lastTurnOutcome: { status: "completed", finishedAt: at },
      terminalActivity: { terminalId: "term-1", report: { state: "idle" }, updatedAt: at }, startedAt: 1, updatedAt: 2, runningSubagents: 0 }),
  ];
  expect(scopedOps(ops, host, () => undefined)).toHaveLength(3);
  expect(scopedOps([op("sessions", "chat-1", { chatId: "chat-1", deviceId: host.deviceId!, lastTurnOutcome: "completed" })], host, () => undefined))
    .toBeUndefined();
});

it("allows every column the native registry writes", () => {
  const native = readFileSync(new URL("../../crates/doc/src/registry.rs", import.meta.url), "utf8").split("#[cfg(test)]")[0]!;
  const written = new Set([...native.matchAll(/\(\s*"([a-zA-Z][a-zA-Z0-9]*)"\s*,/g), ...native.matchAll(/^\s*"([a-zA-Z][a-zA-Z0-9]*)",\s*$/gm)]
    .map(match => match[1]!));
  const allowed = new Set(Object.values(registryColumns).flatMap(kind => Object.keys(kind)));
  expect([...written].filter(field => !allowed.has(field)).sort()).toEqual([]);
});
