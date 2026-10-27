import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const edgeDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const proofName = "staging-proof.json";
export const legacy = Object.freeze({
  sourceCommit: "ae2e4a94d909a3036ec5608e349140d7fc3d5168",
  liveVersion: "be220cb6-052c-4646-b92b-1401c8a29a50",
  jsSha256: "90eec863218b90ac58e35ab3ccc5b4a9f94e8126fbd3bea4dff826b2d26bc36c",
  wasmSha256: "525d62b10e3b346a2e87af6766dba51e97af482164d984a085d8268b5632951e",
});
const entries = { candidate: "staging.js", rollback: "staging-rollback.js" };
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

async function filesIn(directory, prefix = "") {
  const files = [];
  for (const item of await readdir(path.join(directory, prefix), { withFileTypes: true })) {
    const name = path.posix.join(prefix, item.name);
    if (item.isDirectory()) files.push(...await filesIn(directory, name));
    else {
      assert(item.isFile(), `Artifact contains a non-regular file: ${name}`);
      if (name !== proofName) files.push(name);
    }
  }
  return files.sort();
}

// Inspect built bytes, not filenames alone: the recovered source must be embedded
// unchanged in the entry's source map, and its WASM must be a real output module.
export async function inspectArtifact(directory, entry, expected = legacy) {
  const files = {};
  for (const name of await filesIn(directory)) files[name] = sha256(await readFile(path.join(directory, name)));
  assert(files[entry], `Missing explicit Worker entry ${entry}`);
  const map = JSON.parse(await readFile(path.join(directory, `${entry}.map`), "utf8"));
  const indexes = map.sources.flatMap((source, index) => /(^|\/)legacy\/ae2\/index\.js$/.test(source) ? [index] : []);
  assert.equal(indexes.length, 1, "Expected exactly one recovered legacy JS source");
  assert.equal(typeof map.sourcesContent?.[indexes[0]], "string", "Missing legacy sourcesContent");
  assert.equal(sha256(map.sourcesContent[indexes[0]]), expected.jsSha256, "Legacy JS digest mismatch");
  const wasm = Object.keys(files).filter((name) => name.endsWith(".wasm") && files[name] === expected.wasmSha256);
  assert(wasm.length > 0, "Legacy WASM digest mismatch or missing output module");
  return { entry, files, legacySourceMap: `${entry}.map`, legacyWasm: wasm };
}

async function configuration() {
  const config = path.join(edgeDir, "wrangler.jsonc");
  const { experimental_readRawConfig } = await import("wrangler");
  const { rawConfig } = experimental_readRawConfig({ config }, { useRedirectIfAvailable: false });
  assert(rawConfig.env?.staging, "Canonical env.staging is required");
  // Retain the ENTIRE config, not a whitelist of selected bindings. Both builds
  // and deployment use this one file and --env staging, including all migrations.
  return { environment: "staging", sha256: sha256(await readFile(config)), value: rawConfig };
}

async function immutableInputs() {
  assert.equal(sha256(await readFile(path.join(edgeDir, "legacy/ae2/index.js"))), legacy.jsSha256, "Recovered legacy JS changed");
  assert.equal(sha256(await readFile(path.join(edgeDir, "legacy/ae2/2f0f4e9e57402ed68e2f85508ca0967828e35738-loro_wasm_bg.wasm"))), legacy.wasmSha256, "Recovered legacy WASM changed");
}

const runWrangler = (args) => new Promise((resolve, reject) => {
  const child = spawn(path.join(edgeDir, "node_modules/.bin/wrangler"), args, { cwd: edgeDir, stdio: "inherit" });
  child.once("error", reject);
  child.once("exit", (code, signal) => code === 0 ? resolve() : reject(new Error(`Wrangler dry-run failed: ${signal ?? code}`)));
});

export async function sealPair(directory, config, sourceCommit, workflowRun = "") {
  assert.match(sourceCommit, /^[a-f0-9]{40}$/, "A full source commit is required");
  const artifacts = {};
  for (const [kind, entry] of Object.entries(entries)) artifacts[kind] = await inspectArtifact(path.join(directory, kind), entry);
  const proof = { schemaVersion: 1, sourceCommit, workflowRun, legacy, configuration: config, artifacts };
  for (const kind of Object.keys(entries)) await writeFile(path.join(directory, kind, proofName), `${JSON.stringify(proof, null, 2)}\n`);
  return proof;
}

export async function verifyPair(directory, config, sourceCommit) {
  const proof = JSON.parse(await readFile(path.join(directory, "candidate", proofName), "utf8"));
  assert.deepEqual(JSON.parse(await readFile(path.join(directory, "rollback", proofName), "utf8")), proof, "Candidate/rollback proofs differ");
  assert.equal(proof.schemaVersion, 1);
  assert.equal(proof.sourceCommit, sourceCommit, "Artifact source commit mismatch");
  assert.deepEqual(proof.legacy, legacy, "Immutable legacy provenance mismatch");
  assert.deepEqual(proof.configuration, config, "Full staging config/bindings/migrations changed");
  for (const [kind, entry] of Object.entries(entries)) {
    assert.deepEqual(await inspectArtifact(path.join(directory, kind), entry), proof.artifacts[kind], `${kind} sealed bytes changed`);
  }
  return proof;
}

async function main() {
  const [command, output] = process.argv.slice(2);
  assert(output && ["build", "verify"].includes(command), "Usage: staging-artifacts.mjs build|verify OUTPUT_DIRECTORY");
  const directory = path.resolve(output);
  const config = await configuration();
  await immutableInputs();
  if (command === "build") {
    for (const kind of Object.keys(entries)) {
      assert.deepEqual(await configuration(), config, "Staging config changed before dry-run");
      await mkdir(path.join(directory, kind), { recursive: true });
      const source = kind === "candidate" ? "src/staging.ts" : "src/staging-rollback.ts";
      await runWrangler(["deploy", source, "--config", "wrangler.jsonc", "--env", "staging", "--dry-run", "--upload-source-maps", "--outdir", path.join(directory, kind)]);
      assert.deepEqual(await configuration(), config, "Staging config changed during dry-run");
    }
    await immutableInputs();
    await sealPair(directory, config, process.env.GITHUB_SHA, process.env.GITHUB_SERVER_URL && `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`);
  } else {
    await verifyPair(directory, config, process.env.GITHUB_SHA);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}
