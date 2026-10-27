import { expect, it } from "vitest";
import { peerMessageAuthority, peerRoute } from "./peer-route";
import { stampRpcAuthority, type SocketAuthority } from "./authority";
import type { Env } from "./env";

it("permits only exact-source typed peer messages and overwrites forged envelopes", () => {
  const authority: SocketAuthority = { userId: "owner", projectId: "canary", deploymentId: "canary", sessionId: "source", deviceId: "source-device", credential: "device", capabilities: ["session.chat"], writable: false, token: "secret" };
  const target = { targetSessionId: "target", targetDeviceId: "target-device" };
  const command = { kind: "peerMessage", sourceChatId: "source", sourceDeviceId: "source-device", sourceDeploymentId: "canary", text: "hello", threadId: "thread", replyTo: null, hopCount: 0 };
  const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
  const value = { method: "DeliverPeerMessage", params: { chatId: "target", commandId: "command", command, __crewAuthority: { peer: { sourceSessionId: "forged" } } } };
  const peer = peerMessageAuthority(encode(value), authority, target, false);
  expect(peer).toEqual({ sourceSessionId: "source", sourceDeviceId: "source-device", ...target });
  const stamped = JSON.parse(new TextDecoder().decode(stampRpcAuthority(encode(value), authority, peer)));
  expect(stamped.params.__crewAuthority.peer).toEqual(peer);
  expect(stamped.params.__crewAuthority.sessionId).toBe("source");
  expect(stamped.params.__crewAuthority).not.toHaveProperty("token");
  for (const change of [{ sourceChatId: "other" }, { sourceDeviceId: "other" }, { sourceDeploymentId: "shared" }, { hopCount: 9 }, { text: "" }, { kind: "run" }]) {
    expect(peerMessageAuthority(encode({ ...value, params: { ...value.params, command: { ...command, ...change } } }), authority, target, false)).toBeUndefined();
  }
  expect(peerMessageAuthority(encode({ ...value, method: "QueueCommand" }), authority, target, false)).toBeUndefined();
  expect(peerMessageAuthority(encode(value), authority, target, true)).toBeUndefined();
  expect(peerMessageAuthority(encode({ ...value, params: { ...value.params, command: { ...command, replyTo: "original", hopCount: 1 } } }), authority, target, true)).toEqual(peer);
});

it("rejects cross-deployment peer sockets before accessing any room", async () => {
  const identity = { userId: "owner", email: "owner", projectScope: "canary", deploymentId: "canary", credential: "device" as const, capabilities: ["session.chat"], sessionId: "source", targetDeviceId: "source-device" };
  for (const path of ["/session/target/peer/ws?deploymentId=shared", "/device/target-device/ws?role=host&purpose=peer-reply&peerSessionId=target&peerDeploymentId=canary", "/session/target/peer/ws?deploymentId=canary&projectId=shared"]) {
    const response = await peerRoute(new Request(`https://edge.example${path}`, { headers: { upgrade: "websocket" } }), {} as Env, identity);
    expect(response?.status).toBe(403);
  }
});
