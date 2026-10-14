import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// No arguments: wide-vector and concurrent-map CI fixtures. Or: BASELINE.loro DELTA.loro [...]
// --map-fixture runs only the synthetic map check, without the wide-vector baseline.
// Preparation and probing use separate WASM heaps; only the probe has a budget.
const args = process.argv.slice(2);
const internal = ["--prepare", "--prepare-map", "--probe", "--admission"].includes(args[0]);
if (!internal) {
  const mapOnly = args.length === 1 && args[0] === "--map-fixture";
  assert.ok(mapOnly || args.length === 0 || args.length >= 2, "Provide a baseline and at least one delta, --map-fixture, or no arguments");
  for (const mapFixture of mapOnly ? [true] : [false, true]) {
    const directory = mkdtempSync(join(tmpdir(), "crew-workspace-memory-"));
    try {
      for (const mode of [mapFixture ? "--prepare-map" : "--prepare", "--probe", "--admission"]) {
        const child = spawnSync(process.execPath, [fileURLToPath(import.meta.url), mode, directory,
          ...(mapFixture ? [] : args)], { stdio: "inherit" });
        assert.ok(!child.error && child.status === 0, `${mode} failed`);
      }
    } finally { rmSync(directory, { recursive: true, force: true }); }
  }
} else if (args[0] === "--admission") {
  const directory = args[1];
  const { files, before, after, mapFixture } = JSON.parse(readFileSync(join(directory, "manifest.json"), "utf8"));
  const require = createRequire(import.meta.url);
  const { build } = require("esbuild");
  const { Miniflare, convertV4MiniflareOptions } = require("miniflare");
  const source = `
    import { SessionRoom } from ${JSON.stringify(fileURLToPath(new URL("../src/session-room.ts", import.meta.url)))};
    import { initSync } from ${JSON.stringify(require.resolve("loro-crdt/web"))};
    import module from "./loro_wasm_bg.wasm";
    const { memory } = initSync({ module });
    const canonical = v => Array.isArray(v) ? v.map(canonical) : v && typeof v === "object"
      ? Object.fromEntries(Object.keys(v).sort().map(k => [k, canonical(v[k])])) : v;
    const digest = async v => Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256",
      new TextEncoder().encode(JSON.stringify(canonical(v))))), b => b.toString(16).padStart(2, "0")).join("");
    async function fingerprint(doc) {
      const version = doc.oplogVersion();
      const metadata = ${mapFixture} ? doc.getMap("metadata") : undefined;
      try { return { state: await digest(doc.toJSON()), oplogVersion: await digest(Object.fromEntries(version.toJSON())),
        floorFrontiers: await digest(doc.shallowSinceFrontiers().map(({peer, counter}) => [String(peer), counter]).sort()),
        operations: doc.opCount(), ...(metadata ? { mapEditors: await digest(
          ["same-value", "seeded", "deleted"].map(key => metadata.getLastEditor(key) ?? null)) } : {}) }; }
      finally { version.free(); metadata?.free(); }
    }
    export class AdmissionProbe extends SessionRoom {
      async fetch(request) {
        const input = await request.arrayBuffer(), view = new DataView(input), records = [];
        for (let offset = 0; offset < input.byteLength;) {
          const length = view.getUint32(offset, true); offset += 4;
          records.push(new Uint8Array(input, offset, length)); offset += length;
        }
        const publisher = records.pop();
        this.setMeta("chatId", "ws4/memory-smoke"); this.setMeta("roomKind", "workspace");
        this.ctx.storage.transactionSync(() => {
          this.blobs.put("snapshot", records[0]);
          for (const delta of records.slice(1)) this.ctx.storage.sql.exec(
            "INSERT INTO updates(bytes,received_at) VALUES (?,?)", delta, Date.now());
        });
        let doc = await this.ensureDoc(); const before = await fingerprint(doc);
        doc = this.importLoroUpdates(doc, [publisher]).doc;
        const after = await fingerprint(doc);
        doc = this.importLoroUpdates(doc, [publisher]).doc;
        const duplicate = await fingerprint(doc);
        doc.free(); this.doc = undefined;
        const cold = await fingerprint(await this.ensureDoc());
        return Response.json({ before, after, duplicate, cold, wasmBytes: memory.buffer.byteLength });
      }
    }
    export default { fetch(request, env) { return env.ROOM.get(env.ROOM.idFromName("admission")).fetch(request); } };`;
  const compiled = await build({
    stdin: { contents: source, sourcefile: "admission.mjs", resolveDir: process.cwd() },
    bundle: true, format: "esm", platform: "browser", write: false, external: ["node:*", "cloudflare:*"],
    alias: { "loro-crdt": fileURLToPath(new URL("../src/loro-runtime.ts", import.meta.url)) },
    plugins: [{ name: "compiled-wasm", setup(build) {
      build.onResolve({ filter: /\.wasm$/ }, () => ({ path: "./loro_wasm_bg.wasm", external: true }));
    } }]
  });
  const mf = new Miniflare(convertV4MiniflareOptions({ workers: [{
    name: "admission", compatibilityDate: "2026-08-04", compatibilityFlags: ["nodejs_compat"], modulesRoot: directory,
    bindings: { ENVIRONMENT: "local", AUTH_MODE: "dev" },
    durableObjects: { ROOM: { className: "AdmissionProbe", useSQLite: true } },
    modules: [{ type: "ESModule", path: join(directory, "index.mjs"), contents: compiled.outputFiles[0].text },
      { type: "CompiledWasm", path: join(directory, "loro_wasm_bg.wasm"), contents: readFileSync(require.resolve("loro-crdt/web/loro_wasm_bg.wasm")) }]
  }] }));
  try {
    const body = Buffer.concat([...files, join(directory, "offline.loro")].flatMap(path => {
      const bytes = readFileSync(path), header = Buffer.alloc(4);
      header.writeUInt32LE(bytes.length); return [header, bytes];
    }));
    const response = await mf.dispatchFetch("http://local/admit", { method: "POST", body });
    assert.equal(response.status, 200, "Durable Object admission failed");
    const result = await response.json();
    const boundary = ({ state, oplogVersion, floorFrontiers, operations, mapEditors }) => ({
      state, oplogVersion, floorFrontiers, operations, ...(mapFixture ? { mapEditors } : {}) });
    assert.deepEqual(result.before, boundary(before), "Accepted state changed before admission");
    for (const stage of ["after", "duplicate", "cold"]) {
      assert.deepEqual(result[stage], boundary(after), stage + ": admission changed state or retained history");
    }
    assert.ok(result.wasmBytes < 112 * 1048576, "Durable Object admission exceeds Worker memory budget");
    console.log("PASS: Durable Object snapshot admission, duplicate delivery, cold replay; WASM " + (result.wasmBytes / 1048576).toFixed(2) + " MiB");
  } finally { await mf.dispose(); }
} else {
  const Instance = WebAssembly.Instance;
  let memory;
  WebAssembly.Instance = new Proxy(Instance, {
    construct(target, args) {
      const instance = Reflect.construct(target, args);
      memory = instance.exports.memory;
      return instance;
    }
  });
  let LoroDoc, VersionVector;
  try { ({ LoroDoc, VersionVector } = createRequire(import.meta.url)("loro-crdt")); }
  finally { WebAssembly.Instance = Instance; }

  const directory = args[1];
  const file = (name) => join(directory, name);
  const budget = 112 * 1024 * 1024;
  let mapFixture = args[0] === "--prepare-map";
  function checkMemory(stage) {
    assert.ok(memory instanceof WebAssembly.Memory, "Expected the Loro WASM memory export");
    const mib = (memory.buffer.byteLength / 1048576).toFixed(2);
    assert.ok(memory.buffer.byteLength < budget, `${stage}: WASM ${mib} MiB exceeds the 112 MiB budget (Worker JS needs headroom)`);
    console.log(`${stage}: WASM ${mib} MiB`);
  }
  function replay(doc, bytes) {
    const imported = doc.import(bytes);
    assert.equal(imported.pending?.size ?? 0, 0, "Replay left unresolved causal dependencies");
  }
  function canonical(value) {
    if (Array.isArray(value)) return value.map(canonical);
    if (value && typeof value === "object") {
      return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonical(value[key])]));
    }
    return value;
  }
  // Compare digests, never include private document content in assertion output.
  function digest(value) {
    return createHash("sha256").update(JSON.stringify(canonical(value))).digest("hex");
  }
  function fingerprint(doc) {
    const vectors = [doc.version(), doc.oplogVersion(), doc.shallowSinceVV()];
    const heads = (frontiers) => digest(frontiers.map(({ peer, counter }) => [String(peer), counter]).sort());
    try {
      const [stateVersion, oplogVersion, retainedFloor] = vectors.map((vv) => digest(Object.fromEntries(vv.toJSON())));
      assert.equal(stateVersion, oplogVersion, "Materialized state does not cover the oplog");
      assert.equal(heads(doc.frontiers()), heads(doc.oplogFrontiers()), "State and oplog frontiers disagree");
      return {
        state: digest(doc.toJSON()), stateVersion, oplogVersion, retainedFloor,
        frontiers: heads(doc.frontiers()), floorFrontiers: heads(doc.shallowSinceFrontiers()),
        operations: doc.opCount(), changes: doc.changeCount(), ...(mapFixture ? { mapEditors: mapEditors(doc) } : {})
      };
    } finally { for (const vv of vectors) vv.free(); }
  }
  function mapEditors(doc) {
    const metadata = doc.getMap("metadata");
    try { return digest(["same-value", "seeded", "deleted"].map(key => metadata.getLastEditor(key) ?? null)); }
    finally { metadata.free(); }
  }
  function checkMap(doc, expected, stage) {
    const metadata = doc.getMap("metadata");
    try {
      assert.equal(digest([metadata.get("same-value"), metadata.get("seeded"), metadata.get("deleted") ?? null]),
        digest(["visible", expected.seeded, expected.deleted]), `${stage}: concurrent map winners changed`);
      assert.equal(metadata.getLastEditor("same-value"), String(expected.editor), `${stage}: equal-value winner identity changed`);
    } finally { metadata.free(); }
  }
  function edit(doc, key, value) {
    const from = doc.oplogVersion();
    const metadata = doc.getMap("metadata");
    try {
      metadata.set(key, value);
      doc.setNextCommitTimestamp(0);
      doc.commit();
      return doc.export({ mode: "update", from });
    } finally { metadata.free(); from.free(); }
  }

  if (["--prepare", "--prepare-map"].includes(args[0])) {
    let files = args.slice(2), mapChecks;
    if (mapFixture) {
      const seed = new LoroDoc();
      try {
        seed.setPeerId(1);
        edit(seed, "same-value", "floor");
        edit(seed, "seeded", "floor");
        edit(seed, "deleted", "floor");
        writeFileSync(file("offline-base.loro"), seed.export({ mode: "shallow-snapshot", frontiers: seed.frontiers() }));
      } finally { seed.free(); }
      const left = new LoroDoc(), right = new LoroDoc();
      function growHistory(doc, phase) {
        const from = doc.oplogVersion(), metadata = doc.getMap("metadata"), unrelated = doc.getMap("unrequested-map");
        const before = doc.opCount();
        try {
          doc.setPeerId(10);
          // Two retained-history sizes exercise key/container filtering without widening the causal vector.
          for (let round = 0; round < 8000; round++) {
            const value = `${phase}:${round}:${"x".repeat(512)}`;
            metadata.set("retained-history", value);
            unrelated.set("noise", value);
            doc.setNextCommitTimestamp(0); doc.commit();
          }
          assert.equal(doc.opCount() - before, 16000, "Map history growth lost operations");
          return doc.export({ mode: "update", from });
        } finally { from.free(); metadata.free(); unrelated.free(); }
      }
      function branch(doc, peer, winner) {
        const from = doc.oplogVersion(), metadata = doc.getMap("metadata");
        try {
          doc.setPeerId(peer);
          metadata.set("same-value", "intermediate"); metadata.set("same-value", "visible");
          metadata.set("deleted", "temporary");
          if (winner) metadata.delete("deleted"); else metadata.set("deleted", "survives");
          if (winner) metadata.set("seeded", "updated"); else metadata.set("left-only", peer);
          doc.setNextCommitTimestamp(0); doc.commit();
          return doc.export({ mode: "update", from });
        } finally { from.free(); metadata.free(); }
      }
      try {
        replay(left, readFileSync(file("offline-base.loro")));
        growHistory(left, 0);
        replay(right, left.export({ mode: "snapshot" }));
        branch(left, 21, false);
        writeFileSync(file("baseline.loro"), left.export({ mode: "snapshot" }));
        const first = branch(right, 22, true);
        writeFileSync(file("winner.loro"), first);
        replay(left, first);
        const growth = growHistory(left, 1);
        writeFileSync(file("growth.loro"), growth);
        // Join both branches before the second diamond; retained history has doubled.
        replay(right, left.export({ mode: "snapshot" }));
        writeFileSync(file("left.loro"), branch(left, 31, false));
        writeFileSync(file("right.loro"), branch(right, 32, true));
        files = ["baseline.loro", "winner.loro", "growth.loro", "left.loro", "right.loro"].map(file);
        mapChecks = [
          { editor: 21, seeded: "floor", deleted: "survives" },
          { editor: 22, seeded: "updated", deleted: null },
          { editor: 22, seeded: "updated", deleted: null },
          { editor: 31, seeded: "updated", deleted: "survives" },
          { editor: 32, seeded: "updated", deleted: null },
        ];
      } finally { left.free(); right.free(); }
    } else if (!files.length) {
      const seed = new LoroDoc();
      try {
        // A wide retained floor makes equal persistent-vector merges expensive.
        for (let peer = 1; peer <= 450; peer++) {
          seed.setPeerId(peer);
          edit(seed, `device-${peer}`, { id: `device-${peer}`, online: false });
        }
        writeFileSync(file("offline-base.loro"), seed.export({ mode: "shallow-snapshot", frontiers: seed.frontiers() }));
      } finally { seed.free(); }
      const left = new LoroDoc(), right = new LoroDoc();
      try {
        const base = readFileSync(file("offline-base.loro"));
        replay(left, base); replay(right, base);
        left.setPeerId(451); right.setPeerId(452);
        // 2,400 concurrent changes, with two heads joined at every diamond.
        for (let round = 0; round < 1200; round++) {
          const a = edit(left, "left-owner", { deviceId: "device-1", generation: round });
          const b = edit(right, "right-owner", { deviceId: "device-2", generation: round });
          replay(left, b); replay(right, a);
        }
        // A cold import must not cache one wide causal vector per retained node.
        const metadata = left.getMap("metadata");
        try {
          for (let round = 0; round < 200000; round++) {
            left.setPeerId(round % 450 + 1);
            metadata.set("retained-history", round);
            left.setNextCommitTimestamp(0);
            left.commit();
          }
        } finally { metadata.free(); }
        writeFileSync(file("baseline.loro"), left.export({ mode: "snapshot" }));
        writeFileSync(file("left.loro"), edit(left, "left-owner", { deviceId: "device-1", generation: 1200 }));
        writeFileSync(file("right.loro"), edit(right, "right-owner", { deviceId: "device-2", generation: 1200 }));
        files = [file("baseline.loro"), file("left.loro"), file("right.loro")];
      } finally { left.free(); right.free(); }
    } else {
      writeFileSync(file("offline-base.loro"), readFileSync(files[0]));
    }

    const offline = new LoroDoc();
    let offlineVersion;
    try {
      replay(offline, readFileSync(file("offline-base.loro")));
      const vv = offline.oplogVersion();
      try {
        const peers = vv.toJSON();
        let peer = 18446744073709551614n;
        while (peers.has(String(peer))) peer--;
        offline.setPeerId(peer);
      } finally { vv.free(); }
      // Dedicated key leaves the supplied workspace values untouched.
      const metadata = offline.getMap("metadata");
      let key = "__workspace_memory_smoke_offline";
      try { while (metadata.get(key) !== undefined) key += "_"; }
      finally { metadata.free(); }
      writeFileSync(file("offline-delta.loro"), edit(offline, key, "retained"));
      writeFileSync(file("offline.loro"), offline.export({ mode: "snapshot" }));
      const version = offline.oplogVersion();
      try { offlineVersion = Array.from(version.encode()); }
      finally { version.free(); }
    } finally { offline.free(); }

    const reference = new LoroDoc();
    try {
      for (let index = 0; index < files.length; index++) {
        replay(reference, readFileSync(files[index]));
        if (mapChecks) checkMap(reference, mapChecks[index], `Reference ${index}`);
      }
      const before = fingerprint(reference);
      replay(reference, readFileSync(file("offline-delta.loro")));
      const after = fingerprint(reference);
      assert.notEqual(before.state, after.state, "Offline edit must affect visible state");
      assert.notEqual(before.oplogVersion, after.oplogVersion, "Offline edit must advance causal history");
      writeFileSync(file("manifest.json"), JSON.stringify({ files, before, after, offlineVersion, mapFixture, mapChecks }));
    } finally { reference.free(); }
  } else {
    const manifest = JSON.parse(readFileSync(file("manifest.json"), "utf8"));
    const { files, before, after, offlineVersion, mapChecks } = manifest;
    mapFixture = manifest.mapFixture;
    let doc = new LoroDoc();
    function equivalent(expected, stage) {
      assert.deepEqual(fingerprint(doc), expected, `${stage}: state or retained causal history changed`);
      checkMemory(stage);
    }
    function foldAndReopen(expected, stage) {
      writeFileSync(file("folded.loro"), doc.export({ mode: "snapshot" }));
      checkMemory(`${stage} export`);
      doc.free(); doc = new LoroDoc();
      replay(doc, readFileSync(file("folded.loro")));
      equivalent(expected, `${stage} cold reopen`);
    }
    try {
      for (let index = 0; index < files.length; index++) {
        replay(doc, readFileSync(files[index]));
        if (mapChecks) checkMap(doc, mapChecks[index], `Import ${index}`);
        checkMemory(`Import ${index}`);
      }
      equivalent(before, "Complete replay");
      foldAndReopen(before, "Lossless fold");
      replay(doc, readFileSync(file("offline-delta.loro")));
      equivalent(after, "Returning offline edit");
      replay(doc, readFileSync(file("offline-delta.loro")));
      equivalent(after, "Duplicate offline delivery");
      foldAndReopen(after, "Offline fold");
      const from = VersionVector.decode(Uint8Array.from(offlineVersion));
      let catchup;
      try { catchup = doc.export({ mode: "update", from }); }
      finally { from.free(); }
      checkMemory("Offline catch-up export");
      doc.free(); doc = new LoroDoc();
      replay(doc, readFileSync(file("offline.loro")));
      replay(doc, catchup);
      equivalent(after, "Offline writer convergence");
      console.log(`PASS: ${mapFixture ? "concurrent map winners, shallow seeds, tombstones, retained-history growth; " : ""}lossless workspace replay, snapshot folds, cold reopen, and offline round trip`);
    } finally { doc.free(); }
  }
}
