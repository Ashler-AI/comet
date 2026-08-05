#!/usr/bin/env node

import { createHash } from "node:crypto";
import assert from "node:assert/strict";

const CAPABILITIES = Object.freeze([
  "session.read",
  "session.chat",
  "session.control",
  "session.annotate",
  "session.invite",
  "session.files",
  "session.environment",
]);

const sha256 = (value) => createHash("sha256").update(value).digest("hex");
const stable = (value) => JSON.stringify(value, Object.keys(value).sort());

class LocalCollaborationHarness {
  constructor(scope) {
    this.scope = structuredClone(scope);
    this.roomId = `comet-${sha256(stable(scope)).slice(0, 24)}`;
    this.clock = 0;
    this.grants = new Map();
    this.devices = new Map();
    this.sessions = new Map();
    this.publications = [];
    this.commands = [];
    this.audit = [];
  }

  tick() {
    this.clock += 1;
    return `2026-01-01T00:00:${String(this.clock).padStart(2, "0")}.000Z`;
  }

  seedOwner(principal) {
    this.grants.set(principal, new Set(CAPABILITIES));
  }

  invite(actor, principal, capabilities) {
    this.require(actor, "session.invite");
    assert.ok(principal.startsWith("accounts.google.com:"), "grant must bind a verified IAP principal");
    assert.ok(capabilities.every((capability) => CAPABILITIES.includes(capability)));
    this.grants.set(principal, new Set(capabilities));
    return { roomId: this.roomId, principal, capabilities: [...capabilities] };
  }

  join({ principal, deviceId }) {
    this.require(principal, "session.read");
    this.devices.set(deviceId, {
      ownerPrincipal: principal,
      scope: structuredClone(this.scope),
      online: true,
      lastSeenAt: this.tick(),
    });
    return { roomId: this.roomId, cursor: this.publications.length };
  }

  openSession({ principal, deviceId, sessionId }) {
    const device = this.devices.get(deviceId);
    assert.equal(device?.ownerPrincipal, principal);
    this.sessions.set(sessionId, { ownerPrincipal: principal, ownerDeviceId: deviceId });
  }

  publish(principal, publication) {
    this.require(principal, publication.kind === "annotation" ? "session.annotate" : publication.kind === "attachment" ? "session.files" : "session.chat");
    assert.ok(!this.publications.some(({ id }) => id === publication.id), "publications are append-only and uniquely identified");
    const record = Object.freeze({ ...structuredClone(publication), actorPrincipal: principal, createdAt: this.tick() });
    this.publications.push(record);
    return record;
  }

  queueCommand(actorPrincipal, actorDeviceId, targetSessionId, action) {
    this.require(actorPrincipal, "session.control");
    assert.equal(this.devices.get(actorDeviceId)?.ownerPrincipal, actorPrincipal);
    const target = this.sessions.get(targetSessionId);
    assert.ok(target, `unknown session ${targetSessionId}`);
    const command = {
      id: `command-${this.commands.length + 1}`,
      actorDeviceId,
      actorSubject: actorPrincipal,
      grantId: `grant-${sha256(`${this.roomId}:${actorPrincipal}`).slice(0, 12)}`,
      source: "teammate",
      targetSessionId,
      targetDeviceId: target.ownerDeviceId,
      action,
      queuedAt: this.tick(),
      result: "queued",
    };
    this.commands.push(command);
    this.audit.push(structuredClone(command));
    return command.id;
  }

  execute(deviceId, commandId) {
    const command = this.commands.find(({ id }) => id === commandId);
    assert.ok(command, `unknown command ${commandId}`);
    const result = deviceId === command.targetDeviceId ? "executed" : "rejected:not-session-owner";
    if (result === "executed") command.result = result;
    this.audit.push({ ...structuredClone(command), executorDeviceId: deviceId, result, recordedAt: this.tick() });
    return result;
  }

  disconnect(deviceId) {
    this.devices.get(deviceId).online = false;
  }

  reconnect({ principal, deviceId, cursor }) {
    const device = this.devices.get(deviceId);
    assert.equal(device?.ownerPrincipal, principal);
    device.online = true;
    device.lastSeenAt = this.tick();
    return { roomId: this.roomId, cursor: this.publications.length, publications: structuredClone(this.publications.slice(cursor)) };
  }

  require(principal, capability) {
    assert.ok(this.grants.get(principal)?.has(capability), `${principal} lacks ${capability}`);
  }
}

const scope = Object.freeze({ project: "ashler-staging", deployment: "comet-local", thread: "thread-smoke-001" });
const alice = "accounts.google.com:alice@example.test";
const bob = "accounts.google.com:bob@example.test";
const harness = new LocalCollaborationHarness(scope);
harness.seedOwner(alice);

const invite = harness.invite(alice, bob, CAPABILITIES);
assert.equal(invite.roomId, `comet-${sha256(stable(scope)).slice(0, 24)}`);
console.log("PASS invite: verified IAP principal grant is scope-bound");

const joinedA = harness.join({ principal: alice, deviceId: "device-a" });
const joinedB = harness.join({ principal: bob, deviceId: "device-b" });
assert.equal(joinedA.roomId, joinedB.roomId);
harness.openSession({ principal: alice, deviceId: "device-a", sessionId: "session-a" });
harness.openSession({ principal: bob, deviceId: "device-b", sessionId: "session-b" });
console.log("PASS join: two principals and devices share one deterministic scoped room");

harness.publish(alice, { id: "message-a-1", kind: "message", sessionId: "session-a", body: "alpha" });
harness.publish(bob, { id: "message-b-1", kind: "message", sessionId: "session-b", body: "beta", futureField: { opaque: true } });
assert.deepEqual(harness.publications.filter(({ kind }) => kind === "message").map(({ body }) => body), ["alpha", "beta"]);
console.log("PASS transcript: concurrent agent-session publications merge with provenance");

const steerA = harness.queueCommand(bob, "device-b", "session-a", "steer");
assert.equal(harness.execute("device-b", steerA), "rejected:not-session-owner");
assert.equal(harness.execute("device-a", steerA), "executed");
const pauseB = harness.queueCommand(alice, "device-a", "session-b", "pause");
assert.equal(harness.execute("device-b", pauseB), "executed");
assert.ok(harness.audit.some((event) => event.actorSubject === bob && event.actorDeviceId === "device-b" && event.targetSessionId === "session-a" && event.targetDeviceId === "device-a" && event.action === "steer" && event.result === "executed"));
console.log("PASS audit: teammate commands record actor, target session/device, action, and owner-only result");

const persisted = structuredClone(harness.publications);
const checkpoint = harness.publications.length;
harness.disconnect("device-b");
const context = "alpha";
harness.publish(alice, {
  id: "annotation-1",
  kind: "annotation",
  sessionId: "session-a",
  anchor: { targetId: "message-a-1", utf8Range: [0, 5], contextHash: sha256(context) },
  body: "review this",
});
const bytes = Buffer.from("deterministic attachment body", "utf8");
harness.publish(alice, {
  id: "attachment-1",
  kind: "attachment",
  sessionId: "session-a",
  metadata: { name: "evidence.txt", mimeType: "text/plain", size: bytes.length, sha256: sha256(bytes), storageKey: `sha256/${sha256(bytes)}` },
});
const resumed = harness.reconnect({ principal: bob, deviceId: "device-b", cursor: checkpoint });
assert.equal(resumed.roomId, harness.roomId);
assert.ok(resumed.publications.some(({ id, anchor }) => id === "annotation-1" && anchor.targetId === "message-a-1" && anchor.contextHash === sha256(context)));
const attachment = resumed.publications.find(({ id }) => id === "attachment-1");
assert.equal(attachment.metadata.sha256, sha256(bytes));
assert.ok(!("bytes" in attachment), "documents carry attachment metadata, never blob bytes");
assert.ok([...persisted, ...resumed.publications].some(({ id }) => id === "message-a-1"));
assert.equal(persisted.find(({ id }) => id === "message-b-1").futureField.opaque, true);
console.log("PASS reconnect: missed transcript, stable annotation, and attachment metadata replay without blob bytes");

console.log("PASS headless collaboration smoke complete");
