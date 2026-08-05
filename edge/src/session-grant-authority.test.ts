import { describe, expect, it, vi } from "vitest";
import { AuthGrant } from "./grant-authority";
import { enforceDeviceGrantAuthority, SessionRoom } from "./session-room";
import { GRANT_EVENT_HEADER, type Env } from "./env";

const NOW = 1_000_000;
const attachedGrant = { grantId: "grant-1", grantExpiresAt: NOW + 60_000 };

const socket = () => ({ close: vi.fn() });

describe("open session socket grant authority", () => {
  it("closes an authorized open socket when its attached expiry arrives", async () => {
    const ws = socket();
    const validate = vi.fn(async () => true);

    await expect(
      enforceDeviceGrantAuthority(ws, attachedGrant, NOW, validate)
    ).resolves.toBe(true);
    await expect(
      enforceDeviceGrantAuthority(
        ws,
        attachedGrant,
        attachedGrant.grantExpiresAt,
        validate
      )
    ).resolves.toBe(false);
    expect(validate).toHaveBeenCalledOnce();
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("removes read and write authority from an open socket after revocation", async () => {
    const ws = socket();
    let active = true;
    const validate = async () => active;

    await expect(
      enforceDeviceGrantAuthority(ws, attachedGrant, NOW, validate)
    ).resolves.toBe(true);
    active = false;
    await expect(
      enforceDeviceGrantAuthority(ws, attachedGrant, NOW, validate)
    ).resolves.toBe(false);
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("keeps a current socket authorized after authoritative revalidation", async () => {
    const ws = socket();

    await expect(
      enforceDeviceGrantAuthority(ws, attachedGrant, NOW, async () => true)
    ).resolves.toBe(true);
    expect(ws.close).not.toHaveBeenCalled();
  });

  it("fails closed when the grant authority cannot be reached", async () => {
    const ws = socket();

    await expect(
      enforceDeviceGrantAuthority(ws, attachedGrant, NOW, async () => {
        throw new Error("authority unavailable");
      })
    ).resolves.toBe(false);
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it.each([
    ["missing grant id", { grantExpiresAt: NOW + 60_000 }],
    ["missing expiry", { grantId: "grant-1" }],
    ["invalid grant id", { grantId: "../grant-1", grantExpiresAt: NOW + 60_000 }],
    ["unsafe expiry", { grantId: "grant-1", grantExpiresAt: Number.MAX_VALUE }]
  ])("fails closed for malformed attached grant state: %s", async (_label, state) => {
    const ws = socket();
    const validate = vi.fn(async () => true);

    await expect(
      enforceDeviceGrantAuthority(ws, state, NOW, validate)
    ).resolves.toBe(false);
    expect(validate).not.toHaveBeenCalled();
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("fails closed when the socket attachment cannot be restored", async () => {
    const ws = socket();

    await expect(
      enforceDeviceGrantAuthority(ws, null, NOW, async () => true)
    ).resolves.toBe(false);
    expect(ws.close).toHaveBeenCalledWith(4403, "device grant invalid");
  });

  it("closes only live sockets attached to the revoked grant", async () => {
    const matching = {
      deserializeAttachment: () => attachedGrant,
      close: vi.fn()
    };
    const other = {
      deserializeAttachment: () => ({
        grantId: "grant-2",
        grantExpiresAt: NOW + 60_000
      }),
      close: vi.fn()
    };
    const room = {
      revokedGrants: new Set<string>(),
      ctx: { getWebSockets: () => [matching, other] }
    } as unknown as SessionRoom;

    await SessionRoom.prototype.revokeGrant.call(room, "grant-1");

    expect(matching.close).toHaveBeenCalledWith(4403, "device grant revoked");
    expect(other.close).not.toHaveBeenCalled();
  });
});

describe("grant revocation delivery", () => {
  it("reports only a current matching grant as active", async () => {
    const record: {
      grantId: string;
      projectId: string;
      sessionId: string;
      accessExpiresAt: number;
      revokedAt?: number;
    } = {
      grantId: "grant-1",
      projectId: "ashler-staging",
      sessionId: "session-1",
      accessExpiresAt: Date.now() + 60_000
    };
    const ctx = {
      storage: { get: async () => record }
    } as unknown as DurableObjectState;
    const authority = new AuthGrant(ctx, {} as Env);
    const statusRequest = new Request(
      "https://grant.internal/status?grantId=grant-1",
      { headers: { [GRANT_EVENT_HEADER]: "status" } }
    );

    expect((await authority.fetch(statusRequest.clone())).status).toBe(204);
    record.revokedAt = Date.now();
    expect((await authority.fetch(statusRequest.clone())).status).toBe(401);
    delete record.revokedAt;
    record.accessExpiresAt = Number.NaN;
    expect((await authority.fetch(statusRequest)).status).toBe(401);
  });

  it("notifies the scoped active session room after revocation is durable", async () => {
    const records = new Map<string, unknown>();
    records.set("grant", {
      grantId: "grant-1",
      ownerUserId: "owner@example.com",
      email: "owner@example.com",
      projectId: "ashler-staging",
      deploymentId: "candidate",
      sandboxId: "sandbox-1",
      targetDeviceId: "device-1",
      sessionId: "session-1",
      capabilities: ["session.read", "session.control"],
      grantHash: "unused",
      issuedAt: NOW - 1_000,
      grantExpiresAt: NOW + 60_000,
      accessHash: "unused",
      accessExpiresAt: Date.now() + 60_000
    });
    const notifyRoom = vi.fn(async (_request: Request) => new Response(null, { status: 204 }));
    const roomIdFromName = vi.fn((name: string) => name);
    const ctx = {
      storage: {
        get: async (key: string) => records.get(key),
        put: async (key: string, value: unknown) => records.set(key, value)
      }
    } as unknown as DurableObjectState;
    const env = {
      SESSION_ROOMS: {
        idFromName: roomIdFromName,
        get: (_id: string) => ({ fetch: notifyRoom })
      }
    } as unknown as Env;
    const authority = new AuthGrant(ctx, env);

    const response = await authority.fetch(
      new Request("https://grant.internal/revoke", {
        method: "POST",
        body: JSON.stringify({ ownerUserId: "owner@example.com" })
      })
    );

    expect(response.status).toBe(200);
    expect(records.get("grant")).toMatchObject({ revokedAt: expect.any(Number) });
    expect(notifyRoom).toHaveBeenCalledOnce();
    expect(roomIdFromName).toHaveBeenCalledWith("s3/ashler-staging/session-1");
    const notification = notifyRoom.mock.calls[0]?.[0];
    expect(notification?.headers.get(GRANT_EVENT_HEADER)).toBe("revoke");
    await expect(notification?.json()).resolves.toEqual({ grantId: "grant-1" });
  });
});
