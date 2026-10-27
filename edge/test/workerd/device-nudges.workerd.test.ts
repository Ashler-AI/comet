import { env, runInDurableObject, runDurableObjectAlarm } from "cloudflare:test";
import { expect, it } from "vitest";
import { ensureNudges, enqueueNudge, pendingNudges, acknowledgeNudge, NUDGE_CAP, NUDGE_PAGE } from "../../src/device-nudges";
import { decodeDeviceFrame, encodeDeviceFrame } from "../../src/device-room";

it("migrates legacy receipts, keeps more than 256 wakes, and fences stale acknowledgments", async () => {
  const room = env.TEST_LOG.get(env.TEST_LOG.idFromName(crypto.randomUUID()));
  await runInDurableObject(room, (_instance, state) => {
    const sql = state.storage.sql;
    sql.exec("CREATE TABLE pending_nudges(chat_id TEXT PRIMARY KEY,queued_at INTEGER NOT NULL)");
    sql.exec("INSERT INTO pending_nudges VALUES ('old',1)");
    ensureNudges(sql);
    const old = pendingNudges(sql)[0];
    expect(old.token).not.toBe("");
    enqueueNudge(sql, "old");
    acknowledgeNudge(sql, "old", old.token);
    expect(pendingNudges(sql).find(r => r.chat_id === "old")).toBeDefined();
    for (let i=0; i<NUDGE_CAP-1; i++) expect(enqueueNudge(sql, `chat-${i}`)).toBe(true);
    expect(enqueueNudge(sql, "overflow")).toBe(false);
    expect([...sql.exec("SELECT count(*) AS n FROM pending_nudges")][0].n).toBe(NUDGE_CAP);
    expect(pendingNudges(sql)[0].chat_id).toBe("*");
  });
});

it("replays unacknowledged frames and retires only the matching token", async () => {
  const room = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(crypto.randomUUID()));
  const headers = { "x-zeron-auth-user": "owner", upgrade: "websocket" };
  const response = await room.fetch("https://test/ws?role=host&nudgeAck=1", { headers });
  expect(response.status).toBe(101);
  const ws = response.webSocket!; ws.accept(); ws.binaryType = "arraybuffer";
  const messages: Array<{ chatId: string; token: string }> = [];
  ws.addEventListener("message", e => { const frame=decodeDeviceFrame(new Uint8Array(e.data as ArrayBuffer)); messages.push(JSON.parse(new TextDecoder().decode(frame.payload))); });
  const post = () => room.fetch("https://test/nudge", { method: "POST", headers: { "x-zeron-auth-user": "owner" }, body: JSON.stringify({chatId:"chat"}) });
  const posted = await post();
  expect(posted.status, await posted.text()).toBe(200);
  await expect.poll(() => messages.length).toBe(1);
  const first = messages[0];
  await runDurableObjectAlarm(room);
  await expect.poll(() => messages.length).toBe(2);
  expect(messages[1]).toEqual(first);
  await post();
  await expect.poll(() => messages.length).toBe(3);
  const newer = messages[2];
  ws.send(encodeDeviceFrame({s:"chat", k:"nudgeAck"}, new TextEncoder().encode(JSON.stringify(first))));
  await runInDurableObject(room, (_i, state) => { expect(pendingNudges(state.storage.sql)[0].token).toBe(newer.token); });
  ws.send(encodeDeviceFrame({s:"chat", k:"nudgeAck"}, new TextEncoder().encode(JSON.stringify(newer))));
  await expect.poll(() => runInDurableObject(room, (_i, state) => pendingNudges(state.storage.sql).length)).toBe(0);
  ws.close();
});

// This exercises 300 durable ACKs under concurrent workerd load, not a latency SLA.
it("drains 300 queued wakes through a bounded acknowledgment window", async () => {
  const room = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(crypto.randomUUID()));
  await runInDurableObject(room, (_i, state) => {
    for (let i = 0; i < 300; i++) expect(enqueueNudge(state.storage.sql, `chat-${i}`)).toBe(true);
  });
  const response = await room.fetch("https://test/ws?role=host&nudgeAck=1", { headers: { "x-zeron-auth-user": "owner", upgrade: "websocket" } });
  expect(response.status).toBe(101);
  const ws = response.webSocket!;
  const seen = new Set<string>();
  let receipts: Array<{ chatId: string; token: string }> = [];
  let windowSize = NUDGE_PAGE;
  let windowReady = Promise.withResolvers<void>();
  ws.binaryType = "arraybuffer";
  ws.addEventListener("message", e => {
    const frame = decodeDeviceFrame(new Uint8Array(e.data as ArrayBuffer));
    const receipt = JSON.parse(new TextDecoder().decode(frame.payload));
    seen.add(receipt.chatId);
    receipts.push(receipt);
    if (receipts.length === windowSize) windowReady.resolve();
  });
  ws.accept();
  try {
    for (let acknowledged = 0; acknowledged < 300; acknowledged += NUDGE_PAGE) {
      // Hold ACKs until the entire window arrives; observe the durable queue
      // before releasing the next batch instead of racing each receipt's ACK.
      await windowReady.promise;
      await expect.poll(() => runInDurableObject(room, (_i, state) => ({
        inflight: state.getWebSockets()[0].deserializeAttachment().nudgeInflight.length,
        pending: [...state.storage.sql.exec("SELECT count(*) AS n FROM pending_nudges")][0].n,
      }))).toEqual({ inflight: windowSize, pending: 300 - acknowledged });
      expect(receipts).toHaveLength(windowSize);
      expect(seen.size).toBe(Math.min(acknowledged + NUDGE_PAGE, 300));
      const batch = receipts;
      receipts = [];
      windowSize = Math.min(NUDGE_PAGE, 300 - acknowledged - batch.length);
      windowReady = Promise.withResolvers<void>();
      for (const receipt of batch) {
        ws.send(encodeDeviceFrame({ s: receipt.chatId, k: "nudgeAck" }, new TextEncoder().encode(JSON.stringify(receipt))));
      }
    }
    await expect.poll(() => runInDurableObject(room, (_i, state) => pendingNudges(state.storage.sql).length)).toBe(0);
    expect(seen).toEqual(new Set(Array.from({ length: 300 }, (_, i) => `chat-${i}`)));
  } finally {
    ws.close();
  }
}, 10_000);

it("keeps legacy hosts usable without acknowledgment support", async () => {
  const room = env.DEVICE_ROOMS.get(env.DEVICE_ROOMS.idFromName(crypto.randomUUID()));
  const response = await room.fetch("https://test/ws?role=host", { headers: { "x-zeron-auth-user": "owner", upgrade: "websocket" } });
  const ws = response.webSocket!; ws.accept(); ws.binaryType = "arraybuffer";
  const messages: string[] = [];
  ws.addEventListener("message", e => {
    const frame = decodeDeviceFrame(new Uint8Array(e.data as ArrayBuffer));
    messages.push(JSON.parse(new TextDecoder().decode(frame.payload)).chatId);
  });
  await room.fetch("https://test/nudge", {method:"POST",headers:{"x-zeron-auth-user":"owner"},body:JSON.stringify({chatId:"legacy"})});
  await expect.poll(() => messages).toEqual(["legacy"]);
  await runInDurableObject(room, async (_i, state) => {
    expect(pendingNudges(state.storage.sql)).toEqual([]);
    await state.storage.deleteAlarm();
  });
  ws.close();
});
