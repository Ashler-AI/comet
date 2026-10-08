import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

// Run in a fresh process: node scripts/workspace-replay-memory-smoke.mjs BASELINE.loro DELTA.loro [...]
const files = process.argv.slice(2);
assert.ok(files.length >= 2, "Provide a workspace baseline and at least one returning-writer delta");
const Instance = WebAssembly.Instance;
let memory;
WebAssembly.Instance = new Proxy(Instance, {
  construct(target, args) {
    const instance = Reflect.construct(target, args);
    memory = instance.exports.memory;
    return instance;
  }
});
let LoroDoc;
try { ({ LoroDoc } = createRequire(import.meta.url)("loro-crdt")); }
finally { WebAssembly.Instance = Instance; }
const doc = new LoroDoc();
try {
  for (const file of files) {
    const imported = doc.import(readFileSync(file));
    assert.equal(imported.pending?.size ?? 0, 0, "Replay left unresolved causal dependencies");
    assert.ok(memory instanceof WebAssembly.Memory, "Expected the Loro WASM memory export");
    assert.ok(memory.buffer.byteLength < 128 * 1024 * 1024, "Replay exhausted the Worker's total 128 MiB budget before JS overhead");
  }
  console.log(`PASS: complete workspace replay; WASM linear memory ${(memory.buffer.byteLength / 1048576).toFixed(2)} MiB`);
} finally { doc.free(); }
