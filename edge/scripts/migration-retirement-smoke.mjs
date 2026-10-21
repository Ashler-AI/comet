import assert from "node:assert/strict";
import { Buffer } from "node:buffer";
import { createHash, randomUUID } from "node:crypto";

// Run against wrangler.migration-smoke.jsonc --local with a fresh persistence dir.
// No production endpoint, real credentials, external fetches, or dependencies.
const base = new URL(process.argv[2] ?? "http://127.0.0.1:28765");
assert.equal(process.argv.length <= 3, true, "usage: node scripts/migration-retirement-smoke.mjs [loopback-origin]");
assert.equal(base.protocol, "http:");
assert.ok(["127.0.0.1", "localhost", "[::1]"].includes(base.hostname), "loopback only");
assert.ok(!base.username && !base.password && !base.search && !base.hash && base.pathname === "/", "supply a plain loopback origin");
const PROJECT = "ashler-staging";
const OWNER = "owner@ashler.ai";
const RECEIVED_AT = 1_800_000_000_000;
const blobBase64 = Buffer.alloc(1_500_000, 0xa5).toString("base64");
const updateBase64 = Buffer.alloc(17, 0x5a).toString("base64");

async function request(path, method = "GET", { body, foreign = false } = {}) {
  const target = new URL(path, base);
  assert.equal(target.origin, base.origin, "fixture requests must remain loopback");
  return fetch(target, {
    method, redirect: "error", headers: {
      authorization: "Bearer migration-smoke-local-only", "content-type": "application/json",
      "accept-encoding": "identity",
      ...(foreign ? { "x-smoke-subject": "foreign@ashler.ai" } : {})
    }, ...(body === undefined ? {} : { body: JSON.stringify(body) })
  });
}

async function checkedJson(response, status = 200) {
  assert.equal(response.status, status);
  // Only bounded receipts and inspection metadata use JSON parsing.
  const reader = response.body.getReader();
  const chunks = [];
  let bytes = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    bytes += value.byteLength;
    assert.ok(bytes <= 64 * 1024, "fixture metadata must stay bounded");
    chunks.push(value);
  }
  return JSON.parse(Buffer.concat(chunks, bytes).toString("utf8"));
}

async function verifyStream(response, receipt) {
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-length"), String(receipt.bytes));
  assert.equal(response.headers.get("x-crew-migration-sha256"), receipt.sha256);
  assert.ok(response.headers.get("cache-control")?.split(/\s*,\s*/).includes("no-transform"));
  assert.ok([null, "identity"].includes(response.headers.get("content-encoding")), "canonical export must not be compressed");
  const hash = createHash("sha256");
  let bytes = 0;
  for await (const chunk of response.body) {
    hash.update(chunk);
    bytes += chunk.byteLength;
  }
  assert.equal(bytes, receipt.bytes);
  assert.equal(hash.digest("hex"), receipt.sha256);
}

for (const kind of ["session", "session-unscoped", "workspace"]) {
  const workspace = kind === "workspace";
  const id = workspace ? PROJECT : randomUUID();
  const route = `/fixture/${kind}/${id}`;
  const large = kind === "session";
  const blobRows = large ? 96 : 2;
  const migrationId = randomUUID();
  const exportRoute = `${route}/export?migrationId=${migrationId}`;
  assert.equal((await request(`${route}/seed`, "POST", { body: { large } })).status, 200);
  const before = await checkedJson(await request(`${route}/inspect`));
  const metaValues = workspace
    ? { projectScope: PROJECT, roomKind: "workspace", chatId: `ws4/${PROJECT}`, owner: PROJECT }
    : { ownerUserId: OWNER, projectScope: PROJECT, chatId: id, roomKind: "session",
      ...(kind === "session" ? { deploymentId: PROJECT, scopedSessionId: id } : {}) };
  const meta = Object.keys(metaValues).sort().map((key) => ({ key, value: metaValues[key] }));
  assert.deepEqual(before.source, {
    meta, metaRows: { rows: meta.length },
    blobs: { rows: blobRows, bytes: blobRows * 1_500_000, firstIdx: 0, lastIdx: blobRows - 1, invalidRows: 0 },
    updates: { rows: 1, bytes: 17, firstSeq: 1, lastSeq: 1, invalidRows: 0 }
  });
  assert.deepEqual(before.chunks, { rows: 0, bytes: 0, maxBytes: 0 });
  assert.equal((await request(exportRoute)).status, 409, "GET must not create an export");
  assert.equal((await request(`${route}/legacy-write`, "POST")).status, 410);
  if (!workspace) assert.equal((await request(exportRoute, "POST", { foreign: true })).status, 403);
  const receipt = await checkedJson(await request(exportRoute, "POST"));
  const scope = workspace
    ? { projectId: PROJECT, workspaceId: `ws4/${PROJECT}`, authority: "project", exportedBySubject: OWNER }
    : { ownerSubject: OWNER, projectId: PROJECT, deploymentId: kind === "session" ? PROJECT : null, logicalSessionId: id };
  assert.deepEqual(Object.keys(receipt).sort(), ["version", "format", "sourceGeneration", "migrationId", "scope", "frozen", "frozenAt", "sha256", "bytes"].sort());
  assert.equal(receipt.version, 1);
  assert.equal(receipt.format, "legacy-session-sql-v1");
  assert.equal(receipt.sourceGeneration, workspace ? "workspace" : "session");
  assert.equal(receipt.migrationId, migrationId);
  assert.deepEqual(receipt.scope, scope);
  assert.equal(receipt.frozen, true);
  assert.equal(new Date(receipt.frozenAt).toISOString(), receipt.frozenAt);
  assert.match(receipt.sha256, /^[a-f0-9]{64}$/);
  assert.ok(Number.isSafeInteger(receipt.bytes) && receipt.bytes > 0);
  if (large) assert.ok(receipt.bytes > 128 * 1024 * 1024, "must exceed workerd's entire 128 MiB heap budget");

  // Independently build only hash input fragments from deterministic original
  // bytes. Never concatenate, JSON.parse, or arrayBuffer the exported room.
  const expected = createHash("sha256");
  let expectedBytes = 0;
  const append = (fragment) => { expected.update(fragment); expectedBytes += Buffer.byteLength(fragment); };
  append(`{"schemaVersion":1,"format":"legacy-session-sql-v1","sourceGeneration":${JSON.stringify(receipt.sourceGeneration)},"migrationId":${JSON.stringify(migrationId)},"scope":${JSON.stringify(scope)},"frozenAt":${JSON.stringify(receipt.frozenAt)},"meta":${JSON.stringify(meta)},"blobs":[`);
  for (let idx = 0; idx < blobRows; idx++) {
    append(`${idx === 0 ? "" : ","}{"name":"snapshot","idx":${idx},"bytesBase64":"`);
    append(blobBase64);
    append('"}');
  }
  append(`],"updates":[{"seq":1,"receivedAt":${RECEIVED_AT},"bytesBase64":"${updateBase64}"}]}`);
  assert.equal(receipt.bytes, expectedBytes);
  assert.equal(receipt.sha256, expected.digest("hex"), "digest must represent the exact original snapshot and journal bytes");
  await verifyStream(await request(exportRoute), receipt);
  assert.deepEqual(await checkedJson(await request(exportRoute, "POST")), receipt);
  await verifyStream(await request(exportRoute, "GET", { foreign: workspace }), receipt);
  assert.equal((await request(`${route}/export?migrationId=${randomUUID()}`, "POST")).status, 409);
  assert.equal((await request(`${route}/legacy-write`, "POST")).status, 410);
  assert.equal((await request(`${route}/seed`, "POST", { body: { large: false } })).status, 409, "fixture writes must remain forbidden after freeze");
  if (!workspace) assert.equal((await request(exportRoute, "GET", { foreign: true })).status, 403);
  const after = await checkedJson(await request(`${route}/inspect`));
  assert.deepEqual(after.source, before.source, "every original source byte and metadata row must remain unchanged");
  assert.ok(after.chunks.rows > 0);
  assert.ok(after.chunks.maxBytes > 0 && after.chunks.maxBytes <= 256 * 1024);
  assert.equal(after.chunks.bytes, receipt.bytes);
  assert.equal(after.chunks.rows, Math.ceil(receipt.bytes / (256 * 1024)));
  console.log(JSON.stringify({ kind, bytes: receipt.bytes, sha256: receipt.sha256, chunks: after.chunks.rows, status: "passed" }));
}
console.log("Local workerd/SQLite retirement smoke passed for s4, s3, and workspace.");
