import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { inspectArtifact, legacy, sealPair, verifyPair } from "./staging-artifacts.mjs";

const sourceCommit = "0123456789abcdef0123456789abcdef01234567";
const config = {
  environment: "staging",
  sha256: "fixture-config-digest",
  value: { env: { staging: {
    durable_objects: { bindings: [{ name: "AUTH_GRANTS", class_name: "AuthGrant" }] },
    migrations: [{ tag: "v1", new_sqlite_classes: ["AuthGrant"] }],
    vars: { ENVIRONMENT: "staging" },
    r2_buckets: [{ binding: "BLOBS", bucket_name: "existing-bucket" }],
  } } },
};

async function fixture(t) {
  const directory = await mkdtemp(path.join(os.tmpdir(), "staging-artifacts-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const js = await readFile(new URL("../legacy/ae2/index.js", import.meta.url), "utf8");
  const wasm = await readFile(new URL("../legacy/ae2/2f0f4e9e57402ed68e2f85508ca0967828e35738-loro_wasm_bg.wasm", import.meta.url));
  for (const [kind, entry] of [["candidate", "staging.js"], ["rollback", "staging-rollback.js"]]) {
    const output = path.join(directory, kind);
    await mkdir(output);
    await writeFile(path.join(output, entry), "export default {};\n");
    // A second JS module must never be mistaken for the explicit entry.
    await writeFile(path.join(output, "a-module.js"), "export const value = 1;\n");
    await writeFile(path.join(output, `${entry}.map`), JSON.stringify({
      version: 3, sources: ["../../legacy/ae2/index.js"], sourcesContent: [js], mappings: "",
    }));
    await writeFile(path.join(output, "legacy.wasm"), wasm);
  }
  await sealPair(directory, config, sourceCommit);
  return directory;
}

test("seals and verifies both real filesystem artifacts and immutable legacy bytes", async (t) => {
  const directory = await fixture(t);
  const proof = await verifyPair(directory, config, sourceCommit);
  assert.equal(proof.artifacts.candidate.entry, "staging.js");
  assert.equal(proof.artifacts.rollback.entry, "staging-rollback.js");
  assert.equal(proof.artifacts.candidate.files["legacy.wasm"], legacy.wasmSha256);
  assert.deepEqual(proof.configuration, config);
});

test("rejects modified rollback bytes before a candidate can be used", async (t) => {
  const directory = await fixture(t);
  await writeFile(path.join(directory, "rollback/a-module.js"), "changed bytes");
  await assert.rejects(verifyPair(directory, config, sourceCommit), /rollback sealed bytes changed/);
});

test("requires exact full configuration including bindings, migrations, vars and storage", async (t) => {
  const directory = await fixture(t);
  for (const key of ["durable_objects", "migrations", "vars", "r2_buckets"]) {
    const changed = structuredClone(config);
    delete changed.value.env.staging[key];
    await assert.rejects(verifyPair(directory, changed, sourceCommit), /Full staging config/);
  }
  await assert.rejects(verifyPair(directory, config, "f".repeat(40)), /source commit mismatch/);
});

test("rejects mismatched proofs, missing explicit entry and immutable byte corruption", async (t) => {
  const directory = await fixture(t);
  const candidate = path.join(directory, "candidate");
  await writeFile(path.join(directory, "rollback/staging-proof.json"), "{}");
  await assert.rejects(verifyPair(directory, config, sourceCommit), /proofs differ/);
  await assert.rejects(inspectArtifact(candidate, "index.js"), /Missing explicit Worker entry/);
  await writeFile(path.join(candidate, "staging.js.map"), JSON.stringify({
    sources: ["../legacy/ae2/index.js"], sourcesContent: ["changed legacy code"],
  }));
  await assert.rejects(inspectArtifact(candidate, "staging.js"), /Legacy JS digest mismatch/);
});

test("rejects absent or altered legacy WASM even when JS source remains exact", async (t) => {
  const directory = await fixture(t);
  await writeFile(path.join(directory, "candidate/legacy.wasm"), "changed wasm");
  await assert.rejects(inspectArtifact(path.join(directory, "candidate"), "staging.js"), /Legacy WASM digest mismatch/);
});
