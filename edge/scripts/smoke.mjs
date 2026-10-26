// Native registry/chat2/R2 HTTP smoke against the real Worker. Local-only:
// wrangler dev --local --env local --ip 127.0.0.1 --port 27640 \
//   --var AUTH_MODE:dev --var ENVIRONMENT:local \
//   --var SCAFFOLD_PROJECT_SCOPE:local --var SCAFFOLD_DEPLOYMENT_ID:local \
//   --var 'SCAFFOLD_REQUIRED_CAPABILITIES:session.read session.sync.write session.files' \
//   --var SCAFFOLD_CONTROL_PLANE_URL:http://127.0.0.1:8788 --persist-to <fresh-dir>
// node scripts/smoke.mjs http://127.0.0.1:27640
import assert from "node:assert/strict";
import { randomUUID, randomBytes } from "node:crypto";

const base = new URL(process.argv[2] ?? "http://127.0.0.1:27640");
assert.ok(process.argv.length <= 3, "usage: node scripts/smoke.mjs [loopback-origin]");
assert.equal(base.protocol, "http:");
assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(base.hostname), "local smoke refuses remote hosts");
assert.ok(!base.username && !base.password && !base.search && !base.hash && base.pathname === "/");
const user = `smoke-${randomUUID()}`;
const intruder = `other-${randomUUID()}`;
const chat = randomUUID();
const sockets = [];
const enc = new TextEncoder();
const dec = new TextDecoder();
const FRAME = { hello: 1, state: 2, rowsReq: 3, row: 4, rowsDone: 5, push: 6, ack: 7 };
const encode = (type, header, payload = new Uint8Array()) => {
  const h = enc.encode(JSON.stringify(header));
  const bytes = new Uint8Array(5 + h.length + payload.length);
  bytes[0] = type;
  new DataView(bytes.buffer).setUint32(1, h.length, true);
  bytes.set(h, 5); bytes.set(payload, 5 + h.length);
  return bytes;
};
const decode = (value) => {
  const bytes = new Uint8Array(value);
  assert.ok(bytes.length >= 5);
  const length = new DataView(bytes.buffer, bytes.byteOffset).getUint32(1, true);
  assert.ok(length <= 4096 && 5 + length <= bytes.length);
  return { type: bytes[0], header: JSON.parse(dec.decode(bytes.subarray(5, 5 + length))), payload: bytes.subarray(5 + length) };
};
const http = (path, { method = "GET", token = `${user}@local`, headers = {}, body } = {}) => {
  const url = new URL(path, base);
  assert.equal(url.origin, base.origin);
  return fetch(url, { method, redirect: "error", signal: AbortSignal.timeout(15000),
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...headers }, body });
};
const json = async (response, status = 200) => { assert.equal(response.status, status); return response.json(); };
async function connect(path, binary = false, token = `${user}@local`) {
  const url = new URL(path, base);
  url.protocol = "ws:";
  url.searchParams.set("token", token);
  const ws = new WebSocket(url);
  sockets.push(ws);
  ws.binaryType = "arraybuffer";
  const inbox = [];
  const waiting = new Set();
  let closed = false;
  let error;
  const wake = () => { for (const notify of waiting) notify(); };
  ws.addEventListener("message", (event) => {
    try { inbox.push(binary ? decode(event.data) : JSON.parse(event.data)); }
    catch (failure) { error = failure; }
    wake();
  });
  ws.addEventListener("close", () => { closed = true; wake(); });
  ws.addEventListener("error", () => { error = new Error("WebSocket failed"); wake(); });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { ws.close(); reject(new Error("WebSocket open timeout")); }, 10000);
    ws.addEventListener("open", () => { clearTimeout(timer); resolve(); }, { once: true });
    ws.addEventListener("error", () => { clearTimeout(timer); reject(new Error("WebSocket upgrade failed")); }, { once: true });
  });
  return {
    send: (frame) => ws.send(binary ? frame : JSON.stringify(frame)),
    async next(predicate) {
      const deadline = Date.now() + 10000;
      for (;;) {
        if (error) throw error;
        const index = inbox.findIndex(predicate);
        if (index >= 0) return inbox.splice(index, 1)[0];
        assert.ok(!closed, "WebSocket closed before expected frame");
        assert.ok(Date.now() < deadline, "timed out waiting for native frame");
        await new Promise((resolve) => {
          const done = () => { clearTimeout(timer); waiting.delete(done); resolve(); };
          const timer = setTimeout(done, deadline - Date.now());
          waiting.add(done);
        });
      }
    }
  };
}

try {
  const health = await json(await http("/health", { token: "" }));
  assert.equal(health.auth, "dev"); assert.equal(health.environment, "local");
  const identity = await json(await http("/auth/session"));
  assert.equal(identity.userId, user); assert.equal(identity.projectScope, "local");
  assert.equal(identity.deploymentId, "local"); assert.equal(identity.credential, "dev");
  assert.equal((await http("/registry/local/rows", { token: "" })).status, 401);
  assert.equal((await http("/auth/session", { token: `${user}@foreign` })).status, 401);
  assert.equal((await http("/registry/foreign/rows")).status, 403);
  assert.equal((await http("/registry/local/rows?deploymentId=foreign")).status, 403);
  assert.equal((await http(`/session/${chat}/ws`)).status, 410);
  assert.equal((await http("/workspace/local/ws")).status, 410);
  console.log("PASS local identity, auth/scope denial, obsolete protocol denial");

  const a = await connect("/registry/local/ws?device=peer-a");
  a.send({ t: "hello", cursor: null, device: "peer-a" });
  assert.deepEqual((await a.next((f) => f.t === "state")).rows, []);
  const op = (title, tick) => ({ kind: "chats", id: chat, op: "upsert", hlc: `${String(tick).padStart(13, "0")}-000000-smoke`, set: { title } });
  a.send({ t: "push", batch: "registry-a", ops: [op("from-a", 1)] });
  assert.equal((await a.next((f) => f.t === "ack" && f.batch === "registry-a")).applied, 1);
  const b = await connect("/registry/local/ws?device=peer-b");
  b.send({ t: "hello", cursor: null, device: "peer-b" });
  assert.equal((await b.next((f) => f.t === "state")).rows.find((r) => r.id === chat).fields.title, "from-a");
  b.send({ t: "push", batch: "registry-b", ops: [op("from-b", 2)] });
  assert.equal((await b.next((f) => f.t === "ack" && f.batch === "registry-b")).applied, 1);
  assert.equal((await a.next((f) => f.t === "rows" && f.rows.some((r) => r.fields.title === "from-b"))).rows.find((r) => r.id === chat).fields.title, "from-b");
  a.send({ t: "push", batch: "registry-a-live", ops: [op("from-a-live", 3)] });
  await a.next((f) => f.t === "ack" && f.batch === "registry-a-live");
  await b.next((f) => f.t === "rows" && f.rows.some((r) => r.fields.title === "from-a-live"));
  assert.equal((await json(await http("/registry/local/rows"))).rows.find((r) => r.id === chat).fields.title, "from-a-live");
  assert.deepEqual((await json(await http("/registry/local/rows", { token: `${intruder}@local` }))).rows, []);
  console.log("PASS registry two-peer backfill/live convergence and private-user isolation");

  const ca = await connect(`/chat2/${chat}/ws?device=peer-a`, true);
  ca.send(encode(FRAME.hello, { cursor: 0, device: "peer-a" }));
  assert.equal((await ca.next((f) => f.type === FRAME.state)).header.headSeq, 0);
  const first = randomBytes(1024);
  ca.send(encode(FRAME.push, { batchId: "chat-a" }, first));
  assert.equal((await ca.next((f) => f.type === FRAME.ack)).header.seq, 1);
  const cb = await connect(`/chat2/${chat}/ws?device=peer-b`, true);
  cb.send(encode(FRAME.hello, { cursor: 0, device: "peer-b" }));
  assert.equal((await cb.next((f) => f.type === FRAME.state)).header.headSeq, 1);
  cb.send(encode(FRAME.rowsReq, { after: 0 }));
  assert.deepEqual(Buffer.from((await cb.next((f) => f.type === FRAME.row)).payload), first);
  await cb.next((f) => f.type === FRAME.rowsDone);
  const second = randomBytes(2048);
  cb.send(encode(FRAME.push, { batchId: "chat-b" }, second));
  assert.equal((await cb.next((f) => f.type === FRAME.ack)).header.seq, 2);
  assert.deepEqual(Buffer.from((await ca.next((f) => f.type === FRAME.row)).payload), second);
  const third = randomBytes(4096);
  ca.send(encode(FRAME.push, { batchId: "chat-a-live" }, third));
  assert.equal((await ca.next((f) => f.type === FRAME.ack)).header.seq, 3);
  assert.deepEqual(Buffer.from((await cb.next((f) => f.type === FRAME.row)).payload), third);
  assert.equal((await http(`/chat2/${chat}/stats`, { token: `${intruder}@local` })).status, 403);
  const checkpoint = randomBytes(4096);
  await json(await http(`/chat2/${chat}/checkpoint?seqCovered=3`, {
    method: "POST", headers: { "x-chat2-frontier": Buffer.from([1, 2, 3]).toString("base64") }, body: checkpoint
  }));
  assert.deepEqual(Buffer.from(await (await http(`/chat2/${chat}/checkpoint`)).arrayBuffer()), checkpoint);
  const range = await http(`/chat2/${chat}/checkpoint`, { headers: { range: "bytes=1024-" } });
  assert.equal(range.status, 206);
  assert.deepEqual(Buffer.from(await range.arrayBuffer()), checkpoint.subarray(1024));
  console.log("PASS chat2 two-peer opaque-byte backfill/live relay, ownership, checkpoint/range");

  const blobPath = `/blob/${chat}/${encodeURIComponent("tool#1")}`;
  const attachment = randomBytes(8192);
  await json(await http(blobPath, { method: "PUT", headers: { "content-type": "application/octet-stream" }, body: attachment }));
  assert.deepEqual(Buffer.from(await (await http(blobPath)).arrayBuffer()), attachment);
  assert.equal((await http(blobPath, { method: "HEAD" })).status, 200);
  assert.equal((await http(blobPath, { token: `${intruder}@local` })).status, 404);
  assert.equal((await http(blobPath, { token: "" })).status, 401);
  assert.equal((await http(`/blob/${chat}/bad%2Fpart`, { method: "PUT", body: "x" })).status, 400);
  const retired = await http(`/attachments/${"a".repeat(64)}`, { method: "PUT", body: "must not be discarded as success" });
  assert.equal(retired.status, 410);
  assert.equal((await retired.json()).error, "legacy_attachments_retired");
  console.log("PASS native private R2 tool-blob attachments and traversal/auth isolation");
  console.log("PASS native Edge loopback HTTP/WebSocket smoke");
} finally {
  for (const socket of sockets) socket.close();
}
