import { describe, expect, it } from "vitest";
import type { Verified } from "./auth";
import { sessionRoomKey } from "./index";
import { scopedSessionRoomKey } from "./room-key";

const device = (deploymentId: string): Verified & {
  projectId: string; deploymentId: string; sessionId: string;
} => ({
  userId: "actor", email: "actor@example.com", projectScope: "project-1",
  capabilities: ["session.read"], credential: "device",
  projectId: "project-1", deploymentId, sessionId: "same-session"
});

describe("deployment-scoped SessionRoom keys", () => {
  it("isolates identical session IDs in different deployments", () => {
    const first = sessionRoomKey(device("deployment-a"), "same-session");
    const second = sessionRoomKey(device("deployment-b"), "same-session");
    expect(first).toBe("s4/project-1/deployment-a/same-session");
    expect(second).toBe("s4/project-1/deployment-b/same-session");
    expect(first).not.toBe(second);
    expect(scopedSessionRoomKey("project-1", "deployment-a", "same-session")).toBe(first);
    // A Durable Object name is its physical storage/websocket namespace, so
    // different keys isolate transcript, tail, diff, grants, and attachments.
    expect(new Set([first, second])).toHaveLength(2);
  });

  it("preserves the legacy project/session namespace for unscoped local clients", () => {
    const local: Verified = {
      userId: "actor", email: "actor@example.com", projectScope: "project-1",
      capabilities: ["session.read"], credential: "dev"
    };
    expect(sessionRoomKey(local, "same-session")).toBe("s3/project-1/same-session");
    expect(sessionRoomKey(local, "same-session", "deployment-a"))
      .toBe("s4/project-1/deployment-a/same-session");
  });

  it("fails closed when a scoped credential is used for another session or deployment", () => {
    expect(() => sessionRoomKey(device("deployment-a"), "other-session"))
      .toThrow("device_session_scope_invalid");
    expect(() => sessionRoomKey(device("deployment-a"), "same-session", "deployment-b"))
      .toThrow("device_session_scope_invalid");
  });
});
