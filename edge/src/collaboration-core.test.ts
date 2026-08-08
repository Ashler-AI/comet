import { describe, expect, it } from "vitest";
import {
  controlActorForRpc,
  parseTrustedDeviceGrant,
  requiredCapabilityForRpc
} from "./device-room";

const rpcPayload = (method: string, params: unknown): Uint8Array =>
  new TextEncoder().encode(JSON.stringify({ id: "request-1", method, params }));

const control = (action: string, actorSubject = "iap:chris@example.com"): Uint8Array =>
  rpcPayload("QueueCommand", {
    chatId: "shared-thread",
    command: {
      kind: "control",
      sessionId: "session-kyle",
      ownerDeviceId: "device-kyle",
      actorDeviceId: "device-chris",
      actorSubject,
      grantId: "grant-chris",
      source: "local",
      action: { action }
    }
  });

describe("device relay collaboration authorization", () => {
  it("requires scoped capabilities for every teammate control action", () => {
    const header = { s: "rpc", k: "rpc" };
    expect(requiredCapabilityForRpc(header, control("start"))).toBe("session.chat");
    expect(requiredCapabilityForRpc(header, control("steer"))).toBe("session.chat");
    expect(requiredCapabilityForRpc(header, control("pause"))).toBe("session.control");
    expect(requiredCapabilityForRpc(header, control("resume"))).toBe("session.control");
    expect(requiredCapabilityForRpc(header, control("stop"))).toBe("session.control");
    expect(requiredCapabilityForRpc(header, control("focus"))).toBe("session.control");
    expect(requiredCapabilityForRpc(header, control("annotationCreate"))).toBe("session.annotate");
    expect(requiredCapabilityForRpc(header, control("annotationEdit"))).toBe("session.annotate");
    expect(requiredCapabilityForRpc(header, control("annotationResolve"))).toBe("session.annotate");
    expect(requiredCapabilityForRpc(header, control("environmentLifecycle"))).toBe(
      "session.environment"
    );
  });

  it("treats legacy and malformed command payloads as privileged, never readable", () => {
    const header = { s: "rpc", k: "rpc" };
    const legacy = rpcPayload("QueueCommand", {
      chatId: "shared-thread",
      command: { kind: "interrupt" }
    });
    expect(requiredCapabilityForRpc(header, legacy)).toBe("session.control");
    expect(requiredCapabilityForRpc(header, new Uint8Array([0xff]))).toBe("session.control");
  });

  it("extracts actor provenance for the relay to compare with verified IAP subject", () => {
    expect(controlActorForRpc(control("pause", "iap:kyle@example.com"))).toBe(
      "iap:kyle@example.com"
    );
    expect(controlActorForRpc(rpcPayload("WatchCollaboration", { chatId: "thread" }))).toBe(
      undefined
    );
  });

  it("requires file/environment capabilities for attachment and terminal RPC", () => {
    const header = { s: "rpc", k: "rpc" };
    expect(requiredCapabilityForRpc(header, rpcPayload("ReadAttachmentChunk", {}))).toBe(
      "session.files"
    );
    expect(requiredCapabilityForRpc(header, rpcPayload("OpenTerminal", {}))).toBe(
      "session.environment"
    );
  });
  it("accepts only current server-derived device grants for the exact principal and scope", () => {
    const encoded = JSON.stringify({
      grantId: "grant-1",
      subject: "iap:chris@example.com",
      scope: {
        projectId: "project-a",
        deploymentId: "deployment-a",
        sessionId: "session-a",
        lifecycleEpoch: 1
      },
      sandboxId: "sandbox-a",
      targetDeviceId: "device-a",
      lifecycleEpoch: 1,
      capabilities: ["session.read", "session.control"],
      grantedAt: 10,
      expiresAt: 100,
      revokedAt: null
    });
    expect(
      parseTrustedDeviceGrant(encoded, "iap:chris@example.com", "project-a", 50)?.grantId
    ).toBe("grant-1");
    expect(parseTrustedDeviceGrant(encoded, "iap:kyle@example.com", "project-a", 50)).toBe(
      undefined
    );
    expect(parseTrustedDeviceGrant(encoded, "iap:chris@example.com", "project-b", 50)).toBe(
      undefined
    );
    expect(parseTrustedDeviceGrant(encoded, "iap:chris@example.com", "project-a", 100)).toBe(
      undefined
    );
    const wrongDeployment = JSON.stringify({
      ...JSON.parse(encoded),
      scope: { projectId: "project-a", deploymentId: "", sessionId: "session-a" }
    });
    expect(
      parseTrustedDeviceGrant(wrongDeployment, "iap:chris@example.com", "project-a", 50)
    ).toBe(undefined);
  });

});
