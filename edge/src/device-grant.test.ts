import { DatabaseSync } from "node:sqlite";
import { describe, expect, it, vi } from "vitest";
import worker from "./index";
import {
  DeviceRoom,
  authorizedDeviceSocketRole,
  canonicalGrantEnvelope,
  deviceGrantTargetsRoom,
  decodeDeviceFrame,
  encodeDeviceFrame,
  enforceDeviceHostGrantAuthority,
  parseTrustedDeviceGrant,
  peerCommandAdmission,
  rpcAllowedForDirectPeerReply,
  rpcAllowedForPeerSession,
  rpcAllowedForScopedHost,
  requiredCapabilityForRpc,
} from "./device-room";
import type { Env } from "./env";
import { AUTH_CAPABILITIES_HEADER, AUTH_PROJECT_HEADER, AUTH_USER_HEADER, DEVICE_HOST_AUTH_HEADER, DURABLE_SYNC_PROTOCOL, SYNC_PROTOCOL_HEADER, stripTrustedAuthHeaders } from "./env";

const now = 1_800_000_000_000;
const rawGrant = {
  grantId: "a".repeat(32),
  subject: "developer@ashler.ai",
  scope: {
    projectId: "ashler-staging",
    deploymentId: "deploy-123",
    sessionId: "session-456",
    lifecycleEpoch: 1
  },
  sandboxId: "sandbox-789",
  targetDeviceId: "comet-scaffold-sandbox-789-e1",
  lifecycleEpoch: 1,
  capabilities: ["session.read", "session.control"],
  grantedAt: now - 1_000,
  expiresAt: now + 60_000,
  revokedAt: null
};

describe("ordinary device command admission", () => {
  const client = {
    userId: "owner@example.test", projectScope: "project-a",
    targetDeviceId: "devbox-a", controlSessionId: "chat-a",
    capabilities: ["session.control", "session.chat"], joinedAt: now - 1,
    durableSync: true,
  };
  const host = { ...client, hostAuthorization: "local" as const };
  const request = {
    id: 1, method: "AdmitPeerCommand",
    params: { chatId: "chat-a", commandId: "command-a", command: { kind: "interrupt" } }
  };
  const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));

  it("binds a one-shot command to verified principal, project, device and session", () => {
    expect(peerCommandAdmission(client, host, encode(request), now)?.authority).toEqual({
      subject: client.userId, projectId: client.projectScope, deviceId: "devbox-a",
      chatId: "chat-a", expiresAt: client.joinedAt + 30_000
    });
    for (const changed of [
      { userId: "attacker" }, { projectScope: "foreign" }, { targetDeviceId: "other" },
      { controlSessionId: "other-chat" }, { capabilities: ["session.read"] },
      { controlConsumed: true }, { joinedAt: now - 30_000 }, { grant: rawGrant }
    ]) {
      expect(peerCommandAdmission({ ...client, ...changed }, host, encode(request), now)).toBeUndefined();
    }
    expect(peerCommandAdmission(client, { ...host, hostAuthorization: "sandbox" }, encode(request), now)).toBeUndefined();
    expect(peerCommandAdmission(client, host, encode({ ...request, method: "QueueCommand" }), now)).toBeUndefined();
  });

  it("reads an accepted command only through the same authenticated host/session authority", () => {
    const reader = { ...client, capabilities: ["session.read"] };
    const read = { id: 2, method: "ReadSessionCommand", params: { chatId: "chat-a", commandId: "command-a", targetDeviceId: "devbox-a" } };
    expect(peerCommandAdmission(reader, host, encode(read), now)).toBeDefined();
    for (const changed of [{ userId: "attacker" }, { projectScope: "foreign" }, { targetDeviceId: "other" },
      { controlSessionId: "other-chat" }, { capabilities: [] }, { controlConsumed: true }, { joinedAt: now - 30_000 }]) {
      expect(peerCommandAdmission({ ...reader, ...changed }, host, encode(read), now)).toBeUndefined();
    }
    for (const changed of [{ targetDeviceId: "other" }, { chatId: "other-chat" },
      { roomProjection: { projectId: "project-a", deploymentId: "deployment-a", sessionId: "chat-a" } }]) {
      expect(peerCommandAdmission(reader, host, encode({ ...read, params: { ...read.params, ...changed } }), now)).toBeUndefined();
    }
    expect(peerCommandAdmission(reader, host, encode(request), now)).toBeUndefined();
  });

  it.each(["deploymentId", "controlDeploymentId"] as const)("does not erase an ordinary admission's explicit %s", (field) => {
    for (const value of ["deployment-a", ""]) {
      expect(peerCommandAdmission(client, host, encode({ ...request, params: {
        ...request.params, [field]: value
      } }), now)).toBeUndefined();
    }
    expect(peerCommandAdmission(client, host, encode({ ...request, params: {
      ...request.params, [field]: null
    } }), now)?.authority).toEqual({
      subject: client.userId, projectId: client.projectScope, deviceId: "devbox-a",
      chatId: "chat-a", expiresAt: client.joinedAt + 30_000
    });
  });

  it.each(["deploymentId", "controlDeploymentId"] as const)("rejects direct ordinary socket %s before creating a relay", async (field) => {
    const room = {
      getMeta: (key: string) => key === "projectScope" || key === "owner" ? "project-a" : undefined
    } as unknown as DeviceRoom;
    const response = await DeviceRoom.prototype.fetch.call(room, new Request(
      `https://device.internal/ws?role=client&controlSessionId=11111111-1111-4111-8111-111111111111&${field}=deployment-a`,
      { headers: {
        [AUTH_USER_HEADER]: client.userId,
        [AUTH_PROJECT_HEADER]: "project-a",
        [AUTH_CAPABILITIES_HEADER]: "session.read session.control",
        [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL
      } }
    ));
    expect(response.status).toBe(403);
    expect(await response.text()).toBe("scoped_control_not_supported");
  });

  it("allows typed Local controls without trusting caller grant ids or Scaffold identities", () => {
    const command = {
      kind: "control", source: "local", ownerDeviceId: "devbox-a", sessionId: "agent-a",
      actorSubject: client.userId, grantId: "", action: { action: "steer", prompt: "continue" }
    };
    const typed = { ...request, params: { ...request.params, command } };
    expect(peerCommandAdmission(client, host, encode(typed), now)).toBeDefined();
    for (const changed of [{ source: "scaffold" }, { ownerDeviceId: "other" }, { actorSubject: "attacker" }]) {
      expect(peerCommandAdmission(client, host, encode({ ...typed, params: {
        ...typed.params, command: { ...command, ...changed }
      } }), now)).toBeUndefined();
    }
    expect(peerCommandAdmission({ ...client, capabilities: ["session.control"] }, host, encode(typed), now)).toBeUndefined();
  });

  it("consumes authority once even when relay checks overlap", async () => {
    let attachment = { ...client, role: "client", connId: "desktop", joinedAt: Date.now(), controlConsumed: false };
    const socket = {
      deserializeAttachment: () => attachment,
      serializeAttachment: (value: typeof attachment) => { attachment = value; }
    } as unknown as WebSocket;
    const hostSocket = { deserializeAttachment: () => host } as unknown as WebSocket;
    const deliver = vi.fn();
    const rejectRequest = vi.fn();
    const room = {
      authorizePeerClient: async () => true, authorizeHost: async () => true,
      liveHost: () => hostSocket, deliver, rejectRequest
    } as unknown as DeviceRoom;
    const frame = encodeDeviceFrame({ s: "rpc", k: "rpc" }, encode(request)).buffer as ArrayBuffer;
    await Promise.all([
      DeviceRoom.prototype.webSocketMessage.call(room, socket, frame),
      DeviceRoom.prototype.webSocketMessage.call(room, socket, frame)
    ]);
    expect(deliver).toHaveBeenCalledOnce();
    expect(rejectRequest).toHaveBeenCalledWith(socket, expect.anything(), "peer_command_scope_denied");
    expect(deliver.mock.calls[0][1].k).toBe("peer-command");
  });

  it("denies ordinary raw outcome reads instead of borrowing the host identity", async () => {
    const rejectRequest = vi.fn();
    const deliver = vi.fn();
    const hostSocket = { deserializeAttachment: () => host };
    const room = {
      authorizePeerClient: async () => true, authorizeHost: async () => true,
      liveHost: () => hostSocket, deliver, rejectRequest
    } as unknown as DeviceRoom;
    for (const userId of [client.userId, "foreign-owner"]) {
      const socket = { deserializeAttachment: () => ({ ...client, userId,
        role: "client", capabilities: [...client.capabilities, "session.read"], controlSessionId: undefined }) } as unknown as WebSocket;
      await DeviceRoom.prototype.webSocketMessage.call(room, socket, encodeDeviceFrame({ s: "rpc", k: "rpc" },
        encode({ id: 1, method: "ReadSessionCommand", params: { chatId: "chat-a", commandId: "command-a" } })).buffer as ArrayBuffer);
      expect(rejectRequest).toHaveBeenLastCalledWith(socket, expect.anything(), "peer_command_scope_denied");
    }
    expect(deliver).not.toHaveBeenCalled();
  });

  it("never relays client-forged authority frames", async () => {
    const rejectRequest = vi.fn();
    const deliver = vi.fn();
    const room = { authorizePeerClient: async () => true, rejectRequest, deliver } as unknown as DeviceRoom;
    const socket = { deserializeAttachment: () => ({ ...client, role: "client" }) } as unknown as WebSocket;
    for (const kind of ["grant", "nudge", "peer-command", " relay"]) {
      await DeviceRoom.prototype.webSocketMessage.call(room, socket,
        encodeDeviceFrame({ s: "rpc", k: kind }, encode(request)).buffer as ArrayBuffer);
    }
    expect(rejectRequest).toHaveBeenCalledTimes(4);
    expect(deliver).not.toHaveBeenCalled();
  });
});

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
        lifecycleEpoch: rawGrant.lifecycleEpoch,
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
    ["lifecycle mismatch", { ...rawGrant, lifecycleEpoch: 2 }],
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

  it("requires sandbox host grants and allows only explicit peer clients", () => {
    expect(authorizedDeviceSocketRole("host", true, "sandbox")).toBe("host");
    expect(authorizedDeviceSocketRole("host", true, "local")).toBeUndefined();
    expect(authorizedDeviceSocketRole("host", false, "sandbox")).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", true)).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", true, undefined, true)).toBe("client");
    expect(authorizedDeviceSocketRole("client", false, undefined, true)).toBeUndefined();
    expect(authorizedDeviceSocketRole("client", false, undefined, false, true)).toBe("client");
    expect(authorizedDeviceSocketRole("client", true, undefined, false, true)).toBe("client");
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, rawGrant.targetDeviceId)).toBe(true);
    expect(deviceGrantTargetsRoom(rawGrant.targetDeviceId, "another-device")).toBe(false);
  });

  it("strips spoofed host authority from public input", () => {
    const headers = new Headers({ [DEVICE_HOST_AUTH_HEADER]: "sandbox" });
    stripTrustedAuthHeaders(headers);
    expect(headers.get(DEVICE_HOST_AUTH_HEADER)).toBeNull();
  });
});

describe("durable cold-chat wakeups", () => {
  it("retains SQL wakeups across overflow, failed sends and restarts until exact current-host consumption", async () => {
    const db = new DatabaseSync(":memory:");
    const legacyChatId = "11111111-1111-4111-8111-111111111111";
    const frames: Uint8Array[] = [];
    let failSend = false;
    let attachment = {
      userId: "owner", projectScope: "project-a", capabilities: ["session.read", "session.control"],
      durableSync: true,
      role: "host", connId: "engine", hostAuthorization: "local", joinedAt: Date.now(), superseded: false
    };
    const host = {
      deserializeAttachment: () => attachment,
      serializeAttachment: (value: typeof attachment) => { attachment = value; },
      send: (bytes: Uint8Array) => { if (failSend) throw new Error("closed"); frames.push(bytes); },
      close: () => {}
    } as unknown as WebSocket;
    let sockets: WebSocket[] = [];
    const ctx = {
      storage: {
        sql: { exec: (query: string, ...values: (string | number)[]) => db.prepare(query).all(...values) },
        sync: async () => {}
      },
      getWebSockets: () => sockets,
      getWebSocketAutoResponseTimestamp: () => null,
      setWebSocketAutoResponse: () => {}
    } as unknown as DurableObjectState;
    vi.stubGlobal("WebSocketRequestResponsePair", class {});
    try {
      db.exec("CREATE TABLE pending_nudges (chat_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)");
      db.prepare("INSERT INTO pending_nudges VALUES (?, ?)").run(legacyChatId, 1);
      let room = new DeviceRoom(ctx, {} as Env);
      db.prepare("INSERT INTO meta (key, value) VALUES (?, ?)").run("projectScope", "project-a");
      db.prepare("INSERT INTO meta (key, value) VALUES (?, ?)").run("owner", "project-a");
      const legacy = db.prepare("SELECT * FROM pending_nudges WHERE chat_id = ?").get(legacyChatId);
      expect(legacy?.queued_at).toBe(1);
      expect(typeof legacy?.nudge_id).toBe("string");
      const invalid = await room.fetch(new Request("https://device.internal/nudge", {
        method: "POST", body: JSON.stringify({ chatId: "not-a-uuid" }), headers: {
          [AUTH_USER_HEADER]: "owner", [AUTH_PROJECT_HEADER]: "project-a",
          [AUTH_CAPABILITIES_HEADER]: "session.read session.control",
          [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL
        }
      }));
      expect(invalid.status).toBe(400);
      expect(db.prepare("SELECT * FROM pending_nudges").all()).toEqual([legacy]);
      for (let index = 0; index < 255; index++) {
        const response = await room.fetch(new Request("https://device.internal/nudge", {
          method: "POST", body: JSON.stringify({ chatId: `00000000-0000-4000-8000-${String(index).padStart(12, "0")}` }), headers: {
            [AUTH_USER_HEADER]: "owner", [AUTH_PROJECT_HEADER]: "project-a",
            [AUTH_CAPABILITIES_HEADER]: "session.read session.control",
            [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL
          }
        }));
        expect(response.status).toBe(200);
      }
      const before = db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all();
      const overflow = await room.fetch(new Request("https://device.internal/nudge", {
        method: "POST", body: JSON.stringify({ chatId: "99999999-9999-4999-8999-999999999999" }), headers: {
          [AUTH_USER_HEADER]: "owner", [AUTH_PROJECT_HEADER]: "project-a",
          [AUTH_CAPABILITIES_HEADER]: "session.read session.control",
          [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL
        }
      }));
      expect(overflow.status).toBe(503);
      expect(await overflow.json()).toMatchObject({ delivered: false, queued: false });
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(before);
      sockets = [host];
      failSend = true;
      const failed = await room.fetch(new Request("https://device.internal/nudge", {
        method: "POST", body: JSON.stringify({ chatId: legacyChatId }), headers: {
          [AUTH_USER_HEADER]: "owner", [AUTH_PROJECT_HEADER]: "project-a",
          [AUTH_CAPABILITIES_HEADER]: "session.read session.control",
          [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL
        }
      }));
      expect(await failed.json()).toEqual({ delivered: false, queued: true });
      const pending = db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all();
      expect(pending.find((row) => row.chat_id === legacyChatId)?.queued_at).toBe(1);
      room = new DeviceRoom(ctx, {} as Env);
      await room.deliverHostStartup(host);
      expect(frames).toEqual([]);
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending);
      failSend = false;
      await room.deliverHostStartup(host);
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending);
      const frame = frames.map(decodeDeviceFrame).find((value) => value.header.s === legacyChatId)!;
      const receipt = JSON.parse(new TextDecoder().decode(frame.payload));
      expect(receipt.chatId).toBe(legacyChatId);
      const ack = encodeDeviceFrame({ s: receipt.chatId, k: "nudge-ack" },
        new TextEncoder().encode(JSON.stringify(receipt))).buffer as ArrayBuffer;
      const supersededAck = room.webSocketMessage(host, ack);
      attachment = { ...attachment, superseded: true };
      await supersededAck;
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending);
      attachment = { ...attachment, superseded: false };
      const staleAck = encodeDeviceFrame({ s: receipt.chatId, k: "nudge-ack" },
        new TextEncoder().encode(JSON.stringify({ ...receipt, nudgeId: legacy?.nudge_id }))).buffer as ArrayBuffer;
      await room.webSocketMessage(host, staleAck);
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending);
      const wrongStream = encodeDeviceFrame({ s: "other-chat", k: "nudge-ack" },
        new TextEncoder().encode(JSON.stringify(receipt))).buffer as ArrayBuffer;
      await room.webSocketMessage(host, wrongStream);
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending);
      await room.webSocketMessage(host, ack);
      expect(db.prepare("SELECT * FROM pending_nudges WHERE chat_id = ?").get(legacyChatId)).toBeUndefined();
      expect(db.prepare("SELECT * FROM pending_nudges ORDER BY chat_id").all()).toEqual(pending.filter((row) => row.chat_id !== legacyChatId));
    } finally { db.close(); vi.unstubAllGlobals(); }
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

  it("accepts peer delivery only with chat authority for the granted host session", () => {
    const chatGrant = { ...grant, capabilities: [...grant.capabilities, "session.chat"] };
    const request = payload({
      method: "DeliverPeerMessage",
      params: { chatId: rawGrant.scope.sessionId, command: { kind: "peerMessage" } }
    });
    expect(rpcAllowedForScopedHost(rpc, request, chatGrant)).toBe(true);
    expect(rpcAllowedForScopedHost(rpc, request, grant)).toBe(false);
    expect(
      rpcAllowedForScopedHost(
        rpc,
        payload({
          method: "DeliverPeerMessage",
          params: { chatId: "other-session", command: { kind: "peerMessage" } }
        }),
        chatGrant
      )
    ).toBe(false);
  });

  it("binds a sandbox peer client to its source and target sessions", () => {
    const source = "11111111-1111-4111-8111-111111111111";
    const target = "22222222-2222-4222-8222-222222222222";
    const sourceDevice = "scaffold-source-device";
    const command = {
      method: "DeliverPeerMessage",
      params: {
        chatId: target,
        commandId: "command-1",
        command: {
          kind: "peerMessage",
          sourceChatId: source,
          sourceDeploymentId: "source-deployment",
          sourceDeviceId: sourceDevice,
          threadId: "command-1",
          replyTo: null,
          hopCount: 0,
          text: "status"
        }
      }
    };
    expect(rpcAllowedForPeerSession(payload({ method: "LocalDevice", params: {} }), source, "source-deployment", sourceDevice, target)).toBe(true);
    expect(rpcAllowedForPeerSession(payload(command), source, "source-deployment", sourceDevice, target)).toBe(true);
    expect(rpcAllowedForPeerSession(payload({ ...command, params: { ...command.params, chatId: source } }), source, "source-deployment", sourceDevice, target)).toBe(false);
    expect(rpcAllowedForPeerSession(payload({ ...command, params: { ...command.params, command: { ...command.params.command, sourceChatId: target } } }), source, "source-deployment", sourceDevice, target)).toBe(false);
    expect(rpcAllowedForPeerSession(payload({ ...command, params: { ...command.params, command: { ...command.params.command, sourceDeploymentId: "other" } } }), source, "source-deployment", sourceDevice, target)).toBe(false);
    expect(rpcAllowedForPeerSession(payload({ ...command, params: { ...command.params, command: { ...command.params.command, sourceDeviceId: "other" } } }), source, "source-deployment", sourceDevice, target)).toBe(false);
    expect(rpcAllowedForDirectPeerReply(payload(command), target)).toBe(false);
    const reply = {
      ...command,
      params: {
        ...command.params,
        command: { ...command.params.command, replyTo: "original-command", hopCount: 1 }
      }
    };
    expect(rpcAllowedForDirectPeerReply(payload(reply), target)).toBe(true);
    expect(rpcAllowedForDirectPeerReply(payload({ ...reply, params: { ...reply.params, chatId: source } }), target)).toBe(false);
    expect(requiredCapabilityForRpc(rpc, payload(command))).toBe("session.chat");
  });

  it("allows only the non-mutating exact-device readiness probe", () => {
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "LocalDevice", params: {} }), grant)).toBe(true);
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "LocalDevice", params: { targetDeviceId: "other" } }), grant)).toBe(false);
  });

  it("fences scoped command readback by session, deployment and device", () => {
    const read = { method: "ReadSessionCommand", params: { chatId: rawGrant.scope.sessionId,
      commandId: "accepted-command", targetDeviceId: rawGrant.targetDeviceId,
      roomProjection: { projectId: rawGrant.scope.projectId, deploymentId: rawGrant.scope.deploymentId, sessionId: rawGrant.scope.sessionId } } };
    expect(rpcAllowedForScopedHost(rpc, payload(read), grant)).toBe(true);
    for (const changed of [{ chatId: "other" }, { targetDeviceId: "other" },
      { roomProjection: { ...read.params.roomProjection, deploymentId: "other" } }]) {
      expect(rpcAllowedForScopedHost(rpc, payload({ ...read, params: { ...read.params, ...changed } }), grant)).toBe(false);
    }
    expect(rpcAllowedForScopedHost(rpc, payload(read), { ...grant, capabilities: [] })).toBe(false);
  });

  it("requires file authority and the exact session for attachment uploads", () => {
    const fileGrant = { ...grant, capabilities: [...grant.capabilities, "session.files"] };
    for (const method of ["UploadChunk", "UploadCommit"]) {
      const params = { sessionId: rawGrant.scope.sessionId, uploadId: "image-upload" };
      expect(rpcAllowedForScopedHost(rpc, payload({ method, params }), fileGrant)).toBe(true);
      expect(rpcAllowedForScopedHost(rpc, payload({ method, params }), grant)).toBe(false);
      expect(rpcAllowedForScopedHost(rpc, payload({ method }), fileGrant)).toBe(false);
      expect(rpcAllowedForScopedHost(rpc, payload({ method, params: { ...params, sessionId: "other" } }), fileGrant)).toBe(false);
      expect(rpcAllowedForScopedHost(rpc, payload({ method, params: { ...params, targetDeviceId: "other" } }), fileGrant)).toBe(false);
      expect(rpcAllowedForScopedHost(rpc, payload({ method, params: { ...params, targetDeviceId: rawGrant.targetDeviceId } }), fileGrant)).toBe(true);
    }
  });

  it("denies unrelated document RPCs and generic harness/model discovery", () => {
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "WatchDocMessages" }), grant)).toBe(false);
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "ListHarnesses" }), grant)).toBe(false);
    expect(rpcAllowedForScopedHost(rpc, payload({ method: "ListModels", params: { harness: "omp" } }), grant)).toBe(false);
    expect(rpcAllowedForScopedHost(term, new Uint8Array(), grant)).toBe(false);
  });

  it("keeps authorized traffic on the same connection after request-specific denials", async () => {
    const clientState = {
      role: "client",
      connId: "client-1",
      userId: rawGrant.subject,
      capabilities: ["session.read", "session.control", "session.chat"],
      durableSync: true,
    };
    const client = { deserializeAttachment: () => clientState, send: vi.fn(), close: vi.fn() };
    const host = {
      deserializeAttachment: () => ({ role: "host", durableSync: true, grant }),
      send: vi.fn(),
      close: vi.fn()
    };
    const room = Object.assign(Object.create(DeviceRoom.prototype), {
      liveHost: () => host,
      liveClient: () => client,
      authorizeHost: async () => true
    }) as DeviceRoom;
    const send = async (value: unknown) => {
      const bytes = encodeDeviceFrame(rpc, payload(value));
      await room.webSocketMessage(client as unknown as WebSocket, bytes.buffer as ArrayBuffer);
    };
    const reply = (index: number) => {
      const frame = decodeDeviceFrame(client.send.mock.calls[index][0]);
      return { header: frame.header, value: JSON.parse(new TextDecoder().decode(frame.payload)) };
    };

    await send({ id: 1, method: "LocalDevice", params: {} });
    expect(host.send).toHaveBeenCalledTimes(1);
    await send({ id: 2, method: "WatchCheckoutDiffs", params: {} });
    await send({ id: 3, method: "QueueCommand", params: { command: { sessionId: "other-session" } } });
    await send({ id: 4, method: "QueueCommand", params: { command: {
      kind: "control", actorSubject: "other-user", sessionId: grant.scope.sessionId,
      action: { action: "start" }
    } } });
    await send({ id: 5, method: "UploadChunk", params: { sessionId: grant.scope.sessionId } });
    expect(host.send).toHaveBeenCalledTimes(1);
    for (const [index, err] of [
      "session_scope_denied", "session_scope_denied", "actor_mismatch", "capability_denied"
    ].entries()) {
      expect(reply(index)).toEqual({ header: rpc, value: { id: index + 2, err } });
    }

    const command = { id: 6, method: "QueueCommand", params: { command: { sessionId: grant.scope.sessionId } } };
    await send(command);
    const forwarded = decodeDeviceFrame(host.send.mock.calls[1][0]);
    expect(forwarded.header).toEqual({ ...rpc, from: clientState.connId });
    expect(JSON.parse(new TextDecoder().decode(forwarded.payload))).toEqual(command);
    const response = encodeDeviceFrame({ ...rpc, to: clientState.connId }, payload({ id: 6, ok: { accepted: true } }));
    await room.webSocketMessage(host as unknown as WebSocket, response.buffer as ArrayBuffer);
    expect(reply(4)).toEqual({ header: rpc, value: { id: 6, ok: { accepted: true } } });
    expect(client.close).not.toHaveBeenCalled();
    expect(host.close).not.toHaveBeenCalled();
  });
});

describe("durable relay ingress", () => {
  it("rejects legacy registration and commands without superseding a declared host", async () => {
    const db = new DatabaseSync(":memory:");
    const sockets: Array<{ socket: WebSocket; tags: string[] }> = [];
    class Socket {
      attachment: unknown;
      send = vi.fn(); close = vi.fn();
      serializeAttachment(value: unknown) { this.attachment = value; }
      deserializeAttachment() { return this.attachment; }
    }
    const NativeResponse = Response;
    vi.stubGlobal("Response", class extends NativeResponse {
      constructor(body?: BodyInit | null, init?: ResponseInit) {
        super(body, init?.status === 101 ? { ...init, status: 200 } : init);
        if (init?.status === 101) Object.defineProperty(this, "status", { value: 101 });
      }
    });
    vi.stubGlobal("WebSocketPair", class { 0 = new Socket(); 1 = new Socket(); });
    vi.stubGlobal("WebSocketRequestResponsePair", class {});
    const ctx = {
      storage: { sql: { exec: (query: string, ...values: (string | number)[]) => db.prepare(query).all(...values) }, sync: async () => {} },
      acceptWebSocket: (socket: WebSocket, tags: string[] = []) => { sockets.push({ socket, tags }); },
      getWebSockets: (tag?: string) => sockets.filter((entry) => !tag || entry.tags.includes(tag)).map((entry) => entry.socket),
      getWebSocketAutoResponseTimestamp: () => null, setWebSocketAutoResponse: () => {}
    } as unknown as DurableObjectState;
    try {
      const room = new DeviceRoom(ctx, {} as Env);
      const env = {
        AUTH_MODE: "dev", ENVIRONMENT: "local", SCAFFOLD_PROJECT_SCOPE: "ashler-local",
        SCAFFOLD_CONTROL_PLANE_URL: "http://127.0.0.1:8788",
        SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.control session.environment",
        DEVICE_ROOMS: { idFromName: (name: string) => name, get: () => ({ fetch: (request: Request) => room.fetch(request) }) }
      } as unknown as Env;
      const connect = (role: string, declaration = "") => worker.fetch(new Request(
        `http://127.0.0.1/device/engine/ws?role=${role}&connId=connection${declaration}`, {
          headers: { authorization: "Bearer owner@ashler-local", upgrade: "websocket", [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL }
        }), env);
      for (const declaration of ["", "&syncProtocol=unknown", "&syncProtocol=durable-records-v1&syncProtocol=unknown"]) {
        const rejected = await connect("host", declaration);
        expect(rejected.status).toBe(426);
        expect(await rejected.json()).toMatchObject({ error: "crew_update_required" });
        expect(sockets).toHaveLength(0);
        expect(db.prepare("SELECT * FROM meta").all()).toEqual([]);
      }
      expect((await connect("host", `&syncProtocol=${DURABLE_SYNC_PROTOCOL}`)).status).toBe(101);
      const host = sockets[0].socket as unknown as Socket;
      expect((await connect("client")).status).toBe(426);
      expect((await connect("host")).status).toBe(426);
      expect(host.close).not.toHaveBeenCalled();
      expect(sockets).toHaveLength(1);
      for (const path of ["nudge", "sidecar/repos"]) {
        expect((await worker.fetch(new Request(`http://127.0.0.1/device/engine/${path}`, {
          method: "POST", body: "unread malformed body",
          headers: { authorization: "Bearer owner@ashler-local", [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL }
        }), env)).status).toBe(426);
      }
      expect(db.prepare("SELECT * FROM pending_nudges").all()).toEqual([]);
      expect(db.prepare("SELECT * FROM blobs").all()).toEqual([]);
      expect((await connect("client", `&syncProtocol=${DURABLE_SYNC_PROTOCOL}`)).status).toBe(101);
      const client = sockets[1].socket as unknown as Socket;
      const frame = encodeDeviceFrame({ s: "rpc", k: "rpc" }, new TextEncoder().encode(JSON.stringify({ id: 1, method: "LocalDevice", params: {} }))).buffer as ArrayBuffer;
      await room.webSocketMessage(client as unknown as WebSocket, frame);
      expect(host.send).toHaveBeenCalledOnce();
      host.send.mockClear();
      const legacy = new Socket();
      legacy.serializeAttachment({ ...(client.attachment as object), durableSync: undefined });
      await room.webSocketMessage(legacy as unknown as WebSocket, frame);
      expect(legacy.close.mock.calls.map(([code]) => code)).toEqual([4406]);
      expect(host.send).not.toHaveBeenCalled();
      host.serializeAttachment({ ...(host.attachment as object), durableSync: undefined });
      await room.webSocketMessage(host as unknown as WebSocket, encodeDeviceFrame({ s: "rpc", k: "rpc", to: "connection" }, new Uint8Array()).buffer as ArrayBuffer);
      expect(host.close.mock.calls.map(([code]) => code)).toEqual([4406]);
      expect(client.send).not.toHaveBeenCalled();
    } finally { db.close(); vi.unstubAllGlobals(); }
  });
});
