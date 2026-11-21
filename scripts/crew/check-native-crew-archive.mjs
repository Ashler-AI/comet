#!/usr/bin/env node
// Checks a Crew Linux archive as Scaffold admits one (docs/crew/zgx-thin-port.md,
// "Scaffold sandboxes"). A port of Platform's checks, which this repository's
// workflow can't read (Ashler-AI/ashler-platform is private):
//
// - internal/scaffold/scripts/install-native-crew.mjs (scaffold-staging): the
//   archive's SHA-256, then only `comet`, `crew-web/` and `crew-web-contract.json`
//   extracted from its `comet-<version>-linux-x86_64/` root, every entry a
//   regular file, every browser asset there and nonempty, the two contracts
//   byte-identical and `crew.native.web.v1` with the native paths and IPC,
//   and `comet --help` and `comet headless --help` exiting 0 before
//   readiness is published;
// - native-crew-runner.mjs's readiness marker (`.scaffold/crew/runtime-ready`);
// - validate-comet-release-contract.mjs: `scaffold-manifest.json` is schema 2,
//   `releaseSurface: "scaffold"`, the version, `scaffold.comet-runtime.v2`, and
//   the archive's digest.
//
// Nothing is installed: the extraction is a scratch folder, removed after.
//
//   node scripts/crew/check-native-crew-archive.mjs <archive> <version> <sha256>
//     [--manifest scaffold-manifest.json] [--runtime-version scaffold.comet-runtime.v2]
//     [--no-exec]   (skip running comet: a host that can't run a Linux x86_64 binary)
//
// Zero-install: Node's own modules and the system's tar, as the installer.
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { createReadStream, realpathSync } from "node:fs";
import { chmod, lstat, mkdir, mkdtemp, readFile, readdir, rename, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const WEB_CONTRACT = "crew.native.web.v1";
export const RUNTIME_VERSION = "scaffold.comet-runtime.v2";
const versionPattern = /^[0-9]+\.[0-9]+\.[0-9]+(?:[.-][0-9A-Za-z.-]+)?$/;
const sha256Pattern = /^[0-9a-f]{64}$/i;
const runtimeVersionPattern = /^scaffold\.comet-runtime\.v[1-9][0-9]*$/;
const maxManifestBytes = 64 * 1024;
const assets = ["comet", "crew-web-contract.json", "crew-web/contract.json", "crew-web/server.mjs",
  "crew-web/rpc.mjs", "crew-web/package.json", "crew-web/public/index.html", "crew-web/public/app.js",
  "crew-web/public/style.css"];

export async function sha256File(path) {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(path)) digest.update(chunk);
  return digest.digest("hex");
}

// validate-comet-release-contract.mjs, as Platform's image builder runs it.
export function checkManifest(manifest, { version, runtimeVersion = RUNTIME_VERSION, artifactName, sha256 }) {
  if (!versionPattern.test(version)) throw new Error("expected Comet version is invalid");
  if (!runtimeVersionPattern.test(runtimeVersion)) throw new Error("expected Scaffold runtime version is invalid");
  if (!artifactName || artifactName.includes("/") || artifactName.includes("\\")) throw new Error("expected Comet artifact name is invalid");
  const expected = sha256.toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected)) throw new Error("expected Comet artifact SHA-256 is invalid");
  if (manifest?.schemaVersion !== 2 || manifest?.releaseSurface !== "scaffold") {
    throw new Error("Comet release manifest is not a Scaffold manifest with schema version 2");
  }
  if (manifest.version !== version) throw new Error(`Comet release manifest version is ${manifest.version ?? "<missing>"}; expected ${version}`);
  if (manifest.scaffoldRuntimeVersion !== runtimeVersion) {
    throw new Error(`Comet release runtime is ${manifest.scaffoldRuntimeVersion ?? "<missing>"}; Scaffold requires ${runtimeVersion}`);
  }
  if (manifest.files?.[artifactName]?.sha256?.toLowerCase() !== expected) {
    throw new Error(`Comet release manifest SHA-256 for ${artifactName} does not match the configured digest`);
  }
  return { version, runtimeVersion, artifactName, sha256: expected };
}

// install-native-crew.mjs's installNativeCrew, into a scratch folder.
export async function checkArchive(archive, version, sha256, { exec = true } = {}) {
  if (!versionPattern.test(version ?? "")) throw new Error("Invalid Crew release version");
  if (!sha256Pattern.test(sha256 ?? "")) throw new Error("Invalid Crew archive SHA-256");
  if (await sha256File(archive) !== sha256.toLowerCase()) throw new Error("Crew archive SHA-256 mismatch");
  const prefix = `comet-${version}-linux-x86_64`;
  if (basename(archive) !== `${prefix}.tar.gz`) throw new Error(`Crew archive must be named ${prefix}.tar.gz`);
  // (Beyond the installer, which extracts these three alone: nothing else
  // may sit outside the archive's root.)
  const entries = execFileSync("tar", ["-tzf", resolve(archive)], { encoding: "utf8" }).split("\n").filter(Boolean);
  const stray = entries.map((entry) => entry.replace(/^\.\//, ""))
    .filter((entry) => entry !== "" && entry !== "." && entry !== `${prefix}/` && !entry.startsWith(`${prefix}/`));
  if (stray.length) throw new Error(`Crew archive has entries outside ${prefix}/: ${stray.slice(0, 5).join(", ")}`);
  const scratch = await mkdtemp(join(tmpdir(), "crew-archive-check-"));
  try {
    const stage = join(scratch, "crew");
    await mkdir(stage);
    execFileSync("tar", ["-xzf", resolve(archive), "--strip-components=1", "-C", stage,
      `${prefix}/comet`, `${prefix}/crew-web`, `${prefix}/crew-web-contract.json`], { stdio: "pipe" });
    async function regularTree(directory) {
      for (const entry of await readdir(directory, { withFileTypes: true })) {
        if (entry.isDirectory()) await regularTree(join(directory, entry.name));
        else if (!entry.isFile()) throw new Error(`Crew archive contains non-regular entry: ${entry.name}`);
      }
    }
    await regularTree(stage);
    for (const name of assets) {
      // (The installer's lstat throws on a missing one; the same refusal, named.)
      const metadata = await lstat(join(stage, name)).catch(() => null);
      if (!metadata?.isFile() || metadata.size === 0) throw new Error(`Crew archive is missing regular nonempty ${name}`);
    }
    const contractText = await readFile(join(stage, "crew-web-contract.json"), "utf8");
    if (contractText !== await readFile(join(stage, "crew-web/contract.json"), "utf8")) throw new Error("Crew web contracts disagree");
    const contract = JSON.parse(contractText);
    if (contract.version !== WEB_CONTRACT || contract.installDirectory !== ".scaffold/crew/web" ||
        contract.dataDirectory !== ".scaffold/crew-data" || contract.crewSyncGeneration !== "chat2" ||
        contract.packageDirectory !== "crew-web" || contract.entrypoint !== "web/server.mjs" ||
        contract.ipc?.host !== "127.0.0.1" || contract.ipc?.port !== 39400 || contract.ipc?.transport !== "websocket-json") {
      throw new Error("Crew release does not implement the native web contract");
    }
    await mkdir(join(stage, "bin"), { mode: 0o700 });
    await rename(join(stage, "comet"), join(stage, "bin/comet"));
    await chmod(join(stage, "bin/comet"), 0o700);
    await rename(join(stage, "crew-web"), join(stage, "web"));
    if (exec) {
      execFileSync(join(stage, "bin/comet"), ["--help"], { stdio: "pipe" });
      execFileSync(join(stage, "bin/comet"), ["headless", "--help"], { stdio: "pipe" });
    }
    const ready = { version, sha256: sha256.toLowerCase(), webContract: contract.version };
    await writeFile(join(stage, "runtime-ready"), `${JSON.stringify(ready)}\n`, { mode: 0o600 });
    // native-crew-runner.mjs's admission of that marker.
    const marker = JSON.parse(await readFile(join(stage, "runtime-ready"), "utf8"));
    if (!versionPattern.test(marker.version) || !/^[0-9a-f]{64}$/i.test(marker.sha256) || marker.webContract !== WEB_CONTRACT) {
      throw new Error("Crew readiness marker would not be admitted");
    }
    return { prefix, ready, executed: exec, entries: entries.length };
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}

export async function readManifest(path) {
  const metadata = await stat(path);
  if (!metadata.isFile() || metadata.size > maxManifestBytes) throw new Error(`Comet release manifest must be a regular file no larger than ${maxManifestBytes} bytes`);
  return JSON.parse(await readFile(path, "utf8"));
}

async function main(argv) {
  const positional = [];
  const options = { exec: true };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--manifest") options.manifest = argv[++i];
    else if (argv[i] === "--runtime-version") options.runtimeVersion = argv[++i];
    else if (argv[i] === "--no-exec") options.exec = false;
    else positional.push(argv[i]);
  }
  const [archive, version, sha256] = positional;
  if (!archive || !version || !sha256) {
    throw new Error("usage: check-native-crew-archive.mjs <archive> <version> <sha256> [--manifest file] [--runtime-version v] [--no-exec]");
  }
  const result = await checkArchive(archive, version, sha256, { exec: options.exec });
  if (options.manifest) {
    result.manifest = checkManifest(await readManifest(options.manifest), {
      version, runtimeVersion: options.runtimeVersion ?? RUNTIME_VERSION, artifactName: basename(archive), sha256,
    });
  }
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

// (Real paths: run through a link, such as macOS's /tmp, a check must still
// run rather than pass by doing nothing.)
if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  await main(process.argv.slice(2));
}
