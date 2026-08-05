import { describe, expect, it } from "vitest";
import {
  authorizedDeviceSocketRole,
  canonicalGrantEnvelope,
  deviceGrantTargetsRoom,
  parseTrustedDeviceGrant,
  rpcAllowedForScopedHost
} from "./device-room";

const now = 1_800_000_000_000;
const rawGrant = {
  grantId: "a".repeat(32),
  subject: "developer@ashler.ai",
  scope: {
    projectId: "ashler-staging",
    deploymentId: "deploy-123",
    sessionId: "session-456"
  },
  sandboxId: "sandbox-789",
  targetDeviceId: "comet-scaffold-sandbox-789",
  capabilities: ["session.read", "session.control"],
  grantedAt: now - 1_000,
  expiresAt: now + 60_000,
  revokedAt: null
};

describe("trusted device grants", () => {
  it("emits the exact server-scoped capability envelope", () => {
    const parsed = parseTrustedDeviceGrant(
      JSON.stringify(rawGrant),
      rawGrant.subject,
      rawGrant.scope.projectId,
      now
    );
    expect(parsed).toBeDefined();
    expect(canonicalGrantEnvelope(parsed!)).toEqual({
      grant: {
        id: rawGrant.grantId,
        principalSubject: rawGrant.subject,
        scope: rawGrant.scope,
        capabilities: rawGrant.capabilities,
        sandboxId: rawGrant.sandboxId,
        deviceId: rawGrant.targetDeviceId,
        grantedBy: "comet-edge-device-room",
        grantedAt: rawGrant.grantedAt,
        expiresAt: rawGrant.expiresAt,
        revokedAt: null
      },
      roomId: "s4/ashler-staging/deploy-123/session-456",
      targetDeviceId: rawGrant.targetDeviceId,
      targetSessionId: rawGrant.scope.sessionId
    });
  });

  it.each([
    ["missing deployment", { ...rawGrant, scope: { projectId: "ashler-staging", sessionId: "session-456" } }],
    ["missing sandbox", { ...rawGrant, sandboxId: undefined }],
    ["wrong project", { ...rawGrant, scope: { ...rawGrant.scope, projectId: "other" } }],
    ["revoked", { ...rawGrant, revokedAt: now - 1 }],
    ["expired", { ...rawGrant, expiresAt: now }]
  ])("rejects %s authority", (_name, value) => {
    expect(
      parseTrustedDeviceGrant(JSON.stringify(value), rawGrant.subject, "ashler-staging", now)
    ).toBeUndefined();
  });
});

describe("device host authentication", () => {
  it("derives host role only from a verified device grant", () => {
    expect(authorizedDeviceSocketRole("host", true)).toBe("host");
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, rawGrant.targetDeviceId)).toBe(true);
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, "another-device")).toBe(false);
    expect(authorizedDeviceSocketRole("host", false)).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", true)).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", false)).toBe("client");
  });
});

describe("session-scoped host RPC", () => {
  const grant = parseTrustedDeviceGrant(
    JSON.stringify(rawGrant),
    rawGrant.subject,
    rawGrant.scope.projectId,
    now
  )!;
  const rpc = { s: "stream-1", k: "rpc" } as Parameters<typeof rpcAllowedForScopedHost>[0];
  const term = { s: "stream-1", k: "term" } as Parameters<typeof rpcAllowedForScopedHost>[0];
  const payload = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));

  it("accepts only commands for the granted session", () => {
    expect(
      rpcAllowedForScopedHost(
        rpc,
        payload({ method: "QueueCommand", params: { command: { sessionId: rawGrant.scope.sessionId } } }),
        grant
      )
    ).toBe(true);
    expect(
      rpcAllowedForScopedHost(
        rpc,
        payload({ method: "QueueCommand", params: { command: { sessionId: "other-session" } } }),
        grant
      )
    ).toBe(false);
  });

  it("denies unrelated document RPCs and allows device-local discovery", () => {
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "WatchDocMessages" }), grant)).toBe(false);
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "ListHarnesses" }), grant)).toBe(true);
    expect(rpcAllowedForScopedHost(term, new Uint8Array(), grant)).toBe(false);
  });
});
