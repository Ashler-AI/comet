import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// No arguments: deterministic CI fixture. Or: BASELINE.loro DELTA.loro [...]
// Preparation and probing use separate WASM heaps; only the probe has a budget.
const args = process.argv.slice(2);
const internal = args[0] === "--prepare" || args[0] === "--probe";
if (!internal) {
  assert.ok(args.length === 0 || args.length >= 2, "Provide a baseline and at least one delta, or no arguments");
  const directory = mkdtempSync(join(tmpdir(), "crew-workspace-memory-"));
  try {
    for (const mode of ["--prepare", "--probe"]) {
      const child = spawnSync(process.execPath, [fileURLToPath(import.meta.url), mode, directory, ...args], { stdio: "inherit" });
      assert.ok(!child.error && child.status === 0, `${mode} failed`);
    }
  } finally { rmSync(directory, { recursive: true, force: true }); }
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
        operations: doc.opCount(), changes: doc.changeCount()
      };
    } finally { for (const vv of vectors) vv.free(); }
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

  if (args[0] === "--prepare") {
    let files = args.slice(2);
    if (!files.length) {
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
      for (const path of files) replay(reference, readFileSync(path));
      const before = fingerprint(reference);
      replay(reference, readFileSync(file("offline-delta.loro")));
      const after = fingerprint(reference);
      assert.notEqual(before.state, after.state, "Offline edit must affect visible state");
      assert.notEqual(before.oplogVersion, after.oplogVersion, "Offline edit must advance causal history");
      writeFileSync(file("manifest.json"), JSON.stringify({ files, before, after, offlineVersion }));
    } finally { reference.free(); }
  } else {
    const { files, before, after, offlineVersion } = JSON.parse(readFileSync(file("manifest.json"), "utf8"));
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
      console.log("PASS: lossless workspace replay, snapshot folds, cold reopen, and offline round trip");
    } finally { doc.free(); }
  }
}
