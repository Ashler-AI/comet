import { describe, expect, it } from "vitest";
import {
  AUTH_PROJECT_HEADER,
  AUTH_USER_HEADER,
  SESSION_OWNER_AUTH_HEADER,
  stripTrustedAuthHeaders
} from "./env";
import { SessionRoom, sessionOwnerForConnection } from "./session-room";

const roomWithMeta = (values: Record<string, string>): SessionRoom => {
  const room = Object.create(SessionRoom.prototype) as SessionRoom;
  Object.defineProperty(room, "ctx", {
    value: {
      storage: {
        sql: {
          exec: (query: string, key: string) => {
            if (query !== "SELECT value FROM meta WHERE key = ?") {
              throw new Error(`unexpected SQL in owner check: ${query}`);
            }
            const value = values[key];
            return value === undefined ? [] : [{ value }];
          }
        }
      }
    }
  });
  return room;
};

const ownerCheck = (room: SessionRoom, userId: string, projectScope = "project-a") =>
  room.fetch(
    new Request("https://session.internal/authorize-owner", {
      headers: {
        [AUTH_USER_HEADER]: userId,
        [AUTH_PROJECT_HEADER]: projectScope,
        [SESSION_OWNER_AUTH_HEADER]: "verify"
      }
    })
  );

describe("session owner authority", () => {
  it("binds only the first verified non-device participant without rejecting invitees", () => {
    expect(sessionOwnerForConnection(undefined, "owner@example.com", false, false)).toBe(
      "owner@example.com"
    );
    expect(
      sessionOwnerForConnection("owner@example.com", "invitee@example.com", false, false)
    ).toBe("owner@example.com");
    expect(sessionOwnerForConnection(undefined, "device@example.com", false, true)).toBeUndefined();
    expect(
      sessionOwnerForConnection(undefined, "workspace-user@example.com", true, false)
    ).toBeUndefined();
  });

  it("authorizes the durable owner of the exact project session", async () => {
    const response = await ownerCheck(
      roomWithMeta({ projectScope: "project-a", ownerUserId: "owner@example.com" }),
      "owner@example.com"
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ownsSession: true });
  });

  it("denies an alternate authenticated user", async () => {
    const response = await ownerCheck(
      roomWithMeta({ projectScope: "project-a", ownerUserId: "owner@example.com" }),
      "invitee@example.com"
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ownsSession: false });
  });

  it("denies an unbound session", async () => {
    const response = await ownerCheck(roomWithMeta({ projectScope: "project-a" }), "owner@example.com");
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ownsSession: false });
  });

  it("strips a spoofed internal owner header at the public boundary", async () => {
    const headers = new Headers({
      [AUTH_USER_HEADER]: "owner@example.com",
      [AUTH_PROJECT_HEADER]: "project-a",
      [SESSION_OWNER_AUTH_HEADER]: "verify"
    });
    stripTrustedAuthHeaders(headers);
    expect(headers.get(SESSION_OWNER_AUTH_HEADER)).toBeNull();

    const response = await roomWithMeta({
      projectScope: "project-a",
      ownerUserId: "owner@example.com"
    }).fetch(new Request("https://session.internal/authorize-owner", { headers }));
    expect(response.status).toBe(403);
  });
});
