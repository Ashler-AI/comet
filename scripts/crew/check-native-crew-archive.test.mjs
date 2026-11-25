// node --test scripts/crew/check-native-crew-archive.test.mjs
// The archive checks (install-native-crew.mjs's, ported) against archives built
// here, with a stand-in `comet` that answers its two help invocations.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { checkArchive, checkManifest, sha256File } from "./check-native-crew-archive.mjs";

const version = "0.3.9-zgx.1";
const prefix = `comet-${version}-linux-x86_64`;
// (Crew main's `assets` as of 0.2.157: the page's transcript.js and the
// paused view's detached.mjs among them.)
const webAssets = ["server.mjs", "rpc.mjs", "detached.mjs", "package.json", "contract.json", "public/app.js",
  "public/transcript.js", "public/index.html", "public/style.css"];
const contract = {
  version: "crew.native.web.v1", crewSyncGeneration: "chat2", entrypoint: "web/server.mjs",
  packageDirectory: "crew-web", installDirectory: ".scaffold/crew/web", dataDirectory: ".scaffold/crew-data",
  ipc: { host: "127.0.0.1", port: 39400, transport: "websocket-json" },
  assets: webAssets,
};
const helpOnly = "#!/bin/sh\ncase \"$*\" in \"--help\"|\"headless --help\") echo usage; exit 0 ;; *) exit 64 ;; esac\n";

// An archive as scripts/crew/package-linux-sandbox lays it out, `change`d.
function build(change = () => {}, { name = `${prefix}.tar.gz` } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "crew-archive-test-"));
  const root = join(dir, "stage", prefix);
  mkdirSync(join(root, "crew-web/public"), { recursive: true });
  writeFileSync(join(root, "comet"), helpOnly, { mode: 0o755 });
  for (const asset of webAssets.filter((name) => name !== "contract.json")) {
    writeFileSync(join(root, "crew-web", asset), `// ${asset}\n`);
  }
  const text = `${JSON.stringify(contract, null, 2)}\n`;
  writeFileSync(join(root, "crew-web/contract.json"), text);
  writeFileSync(join(root, "crew-web-contract.json"), text);
  change(root, join(dir, "stage"));
  const archive = join(dir, name);
  execFileSync("tar", ["-czf", archive, "-C", join(dir, "stage"), ...readdirSync(join(dir, "stage"))],
    { env: { ...process.env, COPYFILE_DISABLE: "1" } });
  return { dir, archive };
}

async function check(change, options) {
  const { dir, archive } = build(change, options);
  try {
    return await checkArchive(archive, version, await sha256File(archive));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

test("the packaged layout is admitted, help and all", async () => {
  const result = await check();
  assert.equal(result.prefix, prefix);
  assert.equal(result.executed, true);
  assert.equal(result.ready.webContract, "crew.native.web.v1");
});

test("a changed archive, or one under another name, is refused before anything is read", async () => {
  const { dir, archive } = build();
  try {
    await assert.rejects(checkArchive(archive, version, "0".repeat(64)), /SHA-256 mismatch/);
    await assert.rejects(checkArchive(archive, "0.3.9 zgx", await sha256File(archive)), /Invalid Crew release version/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  await assert.rejects(check(undefined, { name: "comet-0.3.9-zgx.2-linux-x86_64.tar.gz" }), /must be named/);
});

test("every browser asset, both contracts alike, and the native contract", async () => {
  await assert.rejects(check((root) => rmSync(join(root, "crew-web/public/app.js"))), /missing regular nonempty crew-web\/public\/app.js/);
  await assert.rejects(check((root) => writeFileSync(join(root, "crew-web/rpc.mjs"), "")), /nonempty crew-web\/rpc.mjs/);
  await assert.rejects(check((root) => writeFileSync(join(root, "crew-web-contract.json"), "{}")), /contracts disagree/);
  for (const patch of [{ version: "crew.native.web.v2" }, { ipc: { ...contract.ipc, port: 4096 } }, { dataDirectory: ".scaffold/comet-data" }]) {
    const text = JSON.stringify({ ...contract, ...patch });
    await assert.rejects(check((root) => {
      writeFileSync(join(root, "crew-web/contract.json"), text);
      writeFileSync(join(root, "crew-web-contract.json"), text);
    }), /native web contract/);
  }
});

// The archive of 2026-11-25 shipped crew-web's app.js, which imports
// transcript.js, without it, and no detached.mjs for Scaffold's paused view.
test("every file the contract lists, and only plain relative names", async () => {
  await assert.rejects(check((root) => rmSync(join(root, "crew-web/public/transcript.js"))),
    /missing crew-web\/public\/transcript.js, which its contract lists/);
  await assert.rejects(check((root) => rmSync(join(root, "crew-web/detached.mjs"))),
    /missing crew-web\/detached.mjs, which its contract lists/);
  for (const [assets, why] of [[undefined, /lists no assets/], [[], /lists no assets/], [["../comet"], /unsafe asset/],
    [["/etc/passwd"], /unsafe asset/], [["public//app.js"], /unsafe asset/]]) {
    const text = JSON.stringify({ ...contract, assets });
    await assert.rejects(check((root) => {
      writeFileSync(join(root, "crew-web/contract.json"), text);
      writeFileSync(join(root, "crew-web-contract.json"), text);
    }), why);
  }
});

test("regular files only, and nothing outside the root", async () => {
  await assert.rejects(check((root) => {
    rmSync(join(root, "crew-web/public/style.css"));
    symlinkSync("/etc/hosts", join(root, "crew-web/public/style.css"));
  }), /non-regular entry/);
  await assert.rejects(check((_, stage) => writeFileSync(join(stage, "extra.txt"), "x")), /outside/);
});

test("comet must answer --help and headless --help", async () => {
  await assert.rejects(check((root) => {
    writeFileSync(join(root, "comet"), "#!/bin/sh\nexit 1\n");
    chmodSync(join(root, "comet"), 0o755);
  }), /Command failed/);
});

test("the manifest, as Platform's image builder reads it", async () => {
  const { dir, archive } = build();
  try {
    const sha256 = await sha256File(archive);
    const artifactName = `${prefix}.tar.gz`;
    const manifest = { schemaVersion: 2, releaseSurface: "scaffold", scaffoldRuntimeVersion: "scaffold.comet-runtime.v2",
      version, files: { [artifactName]: { sha256 } } };
    assert.equal(checkManifest(manifest, { version, artifactName, sha256 }).sha256, sha256);
    for (const [patch, error] of [
      [{ schemaVersion: 1 }, /schema version 2/], [{ releaseSurface: "desktop" }, /schema version 2/],
      [{ version: "0.3.9-zgx.2" }, /version is/], [{ scaffoldRuntimeVersion: "scaffold.comet-runtime.v1" }, /Scaffold requires/],
      [{ files: { [artifactName]: { sha256: "0".repeat(64) } } }, /does not match/],
    ]) {
      assert.throws(() => checkManifest({ ...manifest, ...patch }, { version, artifactName, sha256 }), error);
    }
    // What package-linux-sandbox wrote, read back the same way.
    writeFileSync(join(dir, "scaffold-manifest.json"), JSON.stringify(manifest));
    assert.deepEqual(JSON.parse(readFileSync(join(dir, "scaffold-manifest.json"), "utf8")).files, manifest.files);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
