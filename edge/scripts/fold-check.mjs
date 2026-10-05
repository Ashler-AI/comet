// Exercises lossless workspace folding against an isolated running `wrangler dev`.
// A fresh reader must recover every committed value and the writer's complete
// causal version after enough updates to cross the durable journal budget.
//
// Usage: node scripts/fold-check.mjs [baseUrl]
import { LoroWebsocketClient } from "loro-websocket";
import { LoroAdaptor } from "loro-adaptors/loro";

import assert from "node:assert/strict";
const base = process.argv[2] ?? "http://127.0.0.1:27640";
const wsBase = base.replace(/^http/, "ws");
const projectScope = "ashler-local";
const token = "alice";
const room = `ws4/${projectScope}`;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const stats = async () => {
  const res = await fetch(`${base}/workspace/${projectScope}/stats`, {
    headers: { authorization: `Bearer ${token}` }
  });
  if (!res.ok) throw new Error(`stats ${res.status}`);
  return res.json();
};

const client = new LoroWebsocketClient({ url: `${wsBase}/workspace/${projectScope}/ws?token=${token}&syncProtocol=durable-records-v1` });
await client.waitConnected();
const adaptor = new LoroAdaptor();
await client.join({ roomId: room, crdtAdaptor: adaptor });
const doc = adaptor.getDoc();
const map = doc.getMap("devices");

// Repeated keys exercise history growth without growing materialized state.
const N = 1700;
for (let i = 0; i < N; i++) {
  map.set(`d${i % 8}`, `beat-${i}`); // tiny presence-adjacent writes
  doc.commit();
}
const reader = new LoroWebsocketClient({ url: `${wsBase}/workspace/${projectScope}/ws?token=${token}&syncProtocol=durable-records-v1` });
try {
  await reader.waitConnected();
  const mirror = new LoroAdaptor();
  await reader.join({ roomId: room, crdtAdaptor: mirror });
  await mirror.waitForReachingServerVersion();
  const expected = map.toJSON();
  const required = doc.oplogVersion();
  const deadline = Date.now() + 30_000;
  for (;;) {
    const recovered = mirror.getDoc();
    const materialized = recovered.version();
    const covered = [...required.toJSON()].every(([peer, end]) =>
      (materialized.get(peer) ?? 0) >= end);
    materialized.free();
    if (covered) {
      assert.deepEqual(recovered.getMap("devices").toJSON(), expected);
      break;
    }
    assert.ok(Date.now() < deadline, "fresh reader did not recover every accepted update");
    await sleep(50);
  }
  required.free();
  console.log("FOLD OK", JSON.stringify({ commits: N, ...(await stats()) }));
} finally {
  reader.close();
  client.close();
}

