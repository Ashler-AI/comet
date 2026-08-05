import { describe, expect, it, vi } from "vitest";
import {
  authorizedDeviceSocketRole,
  canonicalGrantEnvelope,
  deviceGrantTargetsRoom,
  DeviceRoom,
  enforceDeviceHostGrantAuthority,
  parseTrustedDeviceGrant,
  rpcAllowedForScopedHost
} from "./device-room";
import { DEVICE_HOST_AUTH_HEADER, stripTrustedAuthHeaders } from "./env";

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
  it("allows trusted local engines to host while keeping clients grantless", () => {
    expect(authorizedDeviceSocketRole("host", false, "local")).toBe("host");
    expect(authorizedDeviceSocketRole("host", false)).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", false)).toBe("client");
    expect(authorizedDeviceSocketRole("client", false, "local")).toBeUndefined();
  });

  it("requires sandbox host grants and rejects device grants as clients", () => {
    expect(authorizedDeviceSocketRole("host", true, "sandbox")).toBe("host");
    expect(authorizedDeviceSocketRole("host", true, "local")).toBeUndefined();
    expect(authorizedDeviceSocketRole("host", false, "sandbox")).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", true)).toBeUndefined();
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, rawGrant.targetDeviceId)).toBe(true);
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, "another-device")).toBe(false);
  });

  it("strips spoofed host authority from public input", () => {
    const headers = new Headers({ [DEVICE_HOST_AUTH_HEADER]: "sandbox" });
    stripTrustedAuthHeaders(headers);
    expect(headers.get(DEVICE_HOST_AUTH_HEADER)).toBeNull();
  });
});

describe("device host startup", () => {
  it("delivers verified authority before replaying queued work", () => {
    const grant = parseTrustedDeviceGrant(
      JSON.stringify(rawGrant),
      rawGrant.subject,
      rawGrant.scope.projectId,
      now
    )!;
    const delivered: string[] = [];
    const room = {
      deliver: vi.fn(() => delivered.push("grant")),
      replayNudges: vi.fn(() => delivered.push("nudge"))
    } as unknown as DeviceRoom;
    DeviceRoom.prototype.deliverHostStartup.call(
      room,
      {} as WebSocket,
      grant
    );
    expect(delivered).toEqual(["grant", "nudge"]);
  });
});

describe("open device host grant authority", () => {
  it("keeps local hosts outside grant status checks", async () => {
    const ws = { close: vi.fn() };
    const validate = vi.fn(async () => false);

    await expect(
      enforceDeviceHostGrantAuthority(
        ws,
        { hostAuthorization: "local" },
        now,
        validate
      )
    ).resolves.toBe(true);
    expect(validate).not.toHaveBeenCalled();
    expect(ws.close).not.toHaveBeenCalled();
  });

  it("revalidates an active sandbox grant on every routing decision", async () => {
    const ws = { close: vi.fn() };
    let active = true;
    const validate = vi.fn(async () => active);
    const state = {
      hostAuthorization: "sandbox" as const,
      grant: { grantId: rawGrant.grantId, expiresAt: rawGrant.expiresAt }
    };

    await expect(enforceDeviceHostGrantAuthority(ws, state, now, validate)).resolves.toBe(true);
    await expect(enforceDeviceHostGrantAuthority(ws, state, now, validate)).resolves.toBe(true);
    expect(validate).toHaveBeenCalledTimes(2);

    active = false;
    await expect(enforceDeviceHostGrantAuthority(ws, state, now, validate)).resolves.toBe(false);
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("fails closed at cached expiry without consulting status", async () => {
    const ws = { close: vi.fn() };
    const validate = vi.fn(async () => true);

    await expect(
      enforceDeviceHostGrantAuthority(
        ws,
        {
          hostAuthorization: "sandbox",
          grant: { grantId: rawGrant.grantId, expiresAt: now }
        },
        now,
        validate
      )
    ).resolves.toBe(false);
    expect(validate).not.toHaveBeenCalled();
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("closes only the sandbox host attached to an immediately revoked grant", async () => {
    const matching = {
      deserializeAttachment: () => ({
        hostAuthorization: "sandbox",
        grant: { grantId: rawGrant.grantId, expiresAt: rawGrant.expiresAt }
      }),
      close: vi.fn()
    };
    const local = {
      deserializeAttachment: () => ({ hostAuthorization: "local" }),
      close: vi.fn()
    };
    const room = {
      revokedGrants: new Set<string>(),
      ctx: { getWebSockets: () => [matching, local] }
    } as unknown as DeviceRoom;

    await DeviceRoom.prototype.revokeGrant.call(room, rawGrant.grantId);

    expect(matching.close).toHaveBeenCalledWith(4403, "device grant revoked");
    expect(local.close).not.toHaveBeenCalled();
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
