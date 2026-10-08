import { DatabaseSync } from "node:sqlite";
import { describe, expect, it, vi } from "vitest";
import { DeviceRoom, decodeDeviceFrame, encodeDeviceFrame, pickLiveHost } from "./device-room";
import { AUTH_CAPABILITIES_HEADER, AUTH_PROJECT_HEADER, AUTH_USER_HEADER, DEVICE_HOST_AUTH_HEADER,
  DURABLE_SYNC_PROTOCOL, SYNC_PROTOCOL_HEADER, type Env } from "./env";

// The bug this guards: a host whose uplink died silently leaves a socket the
// runtime still lists (no close event ever fires, and the supersede `close()`
// never completes either). Routing to the FIRST host socket pinned the room to
// that corpse — client frames vanished into it while the live host, which had
// reconnected and sat later in the list, received nothing. A non-empty host
// list also suppressed the `host_offline` bounce, so clients hung instead of
// failing fast.
describe("device-room host selection", () => {
  const NOW = 1_000_000_000_000;
  const fresh = NOW - 10_000; // pinged 10s ago
  const corpse = NOW - 10 * 60_000; // silent for 10 minutes

  it("prefers the live host over an older corpse listed first", () => {
    expect(
      pickLiveHost(
        [
          { ws: "corpse", lastSeenAt: corpse },
          { ws: "live", lastSeenAt: fresh }
        ],
        NOW
      )
    ).toBe("live");
  });

  it("still finds the live host when the corpse is listed last", () => {
    expect(
      pickLiveHost(
        [
          { ws: "live", lastSeenAt: fresh },
          { ws: "corpse", lastSeenAt: corpse }
        ],
        NOW
      )
    ).toBe("live");
  });

  it("picks the freshest of several live hosts", () => {
    expect(
      pickLiveHost(
        [
          { ws: "older", lastSeenAt: NOW - 40_000 },
          { ws: "newest", lastSeenAt: NOW - 1_000 },
          { ws: "middle", lastSeenAt: NOW - 20_000 }
        ],
        NOW
      )
    ).toBe("newest");
  });

  it("reports no host when every socket is stale — clients get host_offline", () => {
    expect(
      pickLiveHost(
        [
          { ws: "corpse-a", lastSeenAt: corpse },
          { ws: "corpse-b", lastSeenAt: NOW - 76_000 }
        ],
        NOW
      )
    ).toBeUndefined();
  });

  it("treats a socket attached before this deploy (no timestamps) as dead", () => {
    expect(pickLiveHost([{ ws: "legacy", lastSeenAt: 0 }], NOW)).toBeUndefined();
  });

  it("keeps a just-joined host that has not pinged yet", () => {
    expect(pickLiveHost([{ ws: "joining", lastSeenAt: NOW }], NOW)).toBe("joining");
  });

  it("has no host in an empty room", () => {
    expect(pickLiveHost([], NOW)).toBeUndefined();
  });

  // The runtime still lists a socket while its close is being handled. Counting
  // it as live made `webSocketClose` skip the broadcast, so clients were never
  // told their host had left and sat on a link that would never answer again.
  it("skips the socket whose close is being handled", () => {
    expect(
      pickLiveHost([{ ws: "leaving", lastSeenAt: fresh }], NOW, "leaving")
    ).toBeUndefined();
  });

  it("still finds a successor when the departing socket is excluded", () => {
    expect(
      pickLiveHost(
        [
          { ws: "leaving", lastSeenAt: NOW },
          { ws: "successor", lastSeenAt: fresh }
        ],
        NOW,
        "leaving"
      )
    ).toBe("successor");
  });
});

it("ends old subscriptions on host replacement and routes only the reconnected client", async () => {
  const db = new DatabaseSync(":memory:");
  class Socket {
    attachment: unknown;
    send = vi.fn(); close = vi.fn();
    serializeAttachment(value: unknown) { this.attachment = value; }
    deserializeAttachment() { return this.attachment; }
  }
  const sockets: Array<{ socket: WebSocket; tags: string[] }> = [];
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
    const connect = async (role: "host" | "client") => {
      const response = await room.fetch(new Request(`https://device.internal/ws?role=${role}&connId=viewport`, {
        headers: {
          [AUTH_USER_HEADER]: "owner", [AUTH_PROJECT_HEADER]: "project-a",
          [AUTH_CAPABILITIES_HEADER]: "session.read session.environment",
          [SYNC_PROTOCOL_HEADER]: DURABLE_SYNC_PROTOCOL,
          ...(role === "host" ? { [DEVICE_HOST_AUTH_HEADER]: "local" } : {})
        }
      }));
      expect(response.status).toBe(101);
      return sockets.at(-1)!.socket as unknown as Socket;
    };
    const frame = (to?: string) => encodeDeviceFrame({ s: "watch", k: "rpc", ...(to ? { to } : {}) },
      new TextEncoder().encode(JSON.stringify({ id: 1, method: "WatchChats", params: {} }))).buffer as ArrayBuffer;
    const host = await connect("host");
    const client = await connect("client");
    await room.webSocketMessage(client as unknown as WebSocket, frame());
    expect(host.send).toHaveBeenCalledOnce();
    const successor = await connect("host");
    const ended = decodeDeviceFrame(client.send.mock.calls[0][0]);
    expect(ended.header).toEqual({ s: "", k: " relay" });
    expect(JSON.parse(new TextDecoder().decode(ended.payload))).toEqual({ error: "host_closed" });
    expect(client.close).toHaveBeenCalledWith(1012, "engine reconnected");
    await room.webSocketMessage(client as unknown as WebSocket, frame());
    expect(successor.send).not.toHaveBeenCalled();
    const resumed = await connect("client");
    await room.webSocketClose(host as unknown as WebSocket);
    await room.webSocketClose(client as unknown as WebSocket);
    await room.webSocketMessage(host as unknown as WebSocket, frame("viewport"));
    expect(resumed.send).not.toHaveBeenCalled();
    await room.webSocketMessage(resumed as unknown as WebSocket, frame());
    const routed = decodeDeviceFrame(successor.send.mock.calls[0][0]);
    expect(routed.header).toEqual({ s: "watch", k: "rpc", from: "viewport" });
    await room.webSocketMessage(successor as unknown as WebSocket, frame("viewport"));
    expect(decodeDeviceFrame(resumed.send.mock.calls[0][0]).header).toEqual({ s: "watch", k: "rpc" });
  } finally {
    db.close(); vi.unstubAllGlobals();
  }
});
