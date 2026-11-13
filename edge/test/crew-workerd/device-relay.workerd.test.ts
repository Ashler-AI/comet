import { env } from "cloudflare:test";
import { expect, it } from "vitest";
import { decodeDeviceFrame, encodeDeviceFrame, type DeviceFrameHeader } from "../../src/device-room";

it("keeps the upstream liveness handshake usable without admitting payload-bearing echoes", async () => {
  const room = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(crypto.randomUUID()));
  const headers = { "x-zeron-auth-user": "owner", upgrade: "websocket" };
  const host = (await room.fetch("https://test/ws?role=host", { headers })).webSocket!;
  const client = (await room.fetch("https://test/ws?role=client&connId=caller", { headers })).webSocket!;
  host.accept(); client.accept();
  host.binaryType = client.binaryType = "arraybuffer";
  const received: DeviceFrameHeader[] = [];
  const replies: { header: DeviceFrameHeader; payload: Uint8Array }[] = [];
  host.addEventListener("message", event => {
    const frame = decodeDeviceFrame(new Uint8Array(event.data as ArrayBuffer));
    received.push(frame.header);
    host.send(encodeDeviceFrame({ s: frame.header.s, k: frame.header.k, to: frame.header.from }, frame.payload));
  });
  client.addEventListener("message", event => { replies.push(decodeDeviceFrame(new Uint8Array(event.data as ArrayBuffer))); });
  try {
    client.send(encodeDeviceFrame({ s: "echo", k: "echo", from: "forged", to: "other" }, new Uint8Array()));
    await expect.poll(() => replies[0]?.header.k).toBe("echo");
    expect(received[0]).toEqual({ s: "echo", k: "echo", from: "caller" });
    expect(replies[0].payload).toEqual(new Uint8Array());
    client.send(encodeDeviceFrame({ s: "echo", k: "echo" }, new TextEncoder().encode('{"method":"scaffold.control"}')));
    await expect.poll(() => replies[1]?.header.k).toBe(" relay");
    expect(JSON.parse(new TextDecoder().decode(replies[1].payload))).toEqual({ error: "session_scope_denied" });
    expect(received).toHaveLength(1);
  } finally {
    client.close(); host.close();
  }
});
