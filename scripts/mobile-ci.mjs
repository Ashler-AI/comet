import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  closeSync, copyFileSync, createReadStream, existsSync, mkdirSync, openSync,
  readFileSync, readdirSync, realpathSync, writeFileSync,
} from "node:fs";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

// This entrypoint must never compile on a developer workstation.
if (process.env.GITHUB_ACTIONS !== "true" || process.platform !== "darwin" || process.arch !== "arm64") {
  throw new Error("Mobile CI requires an Apple-silicon GitHub Actions macOS runner");
}
process.env.ASHLER_INCREMENTAL_TSC_CHECKS = "false";
const root = process.cwd();
const output = path.join(root, "mobile-artifacts");
const logs = path.join(output, "logs");
const release = path.join(output, "release");
const work = path.join(process.env.RUNNER_TEMP, "crew-mobile");
for (const directory of [logs, release, work]) mkdirSync(directory, { recursive: true });
const environment = process.env.CREW_MOBILE_ENVIRONMENT ?? "staging";
const profiles = {
  staging: { scheme: "Crew Staging", bundleId: "ai.ashler.crew.staging", build: "20", suffix: "-Staging", name: "Crew-Staging" },
  production: { scheme: "Comet", bundleId: "ai.ashler.crew", build: "14", suffix: "", name: "Crew" },
};
if (!Object.hasOwn(profiles, environment)) throw new Error(`Unsupported mobile environment: ${environment}`);
const { scheme, bundleId, build, suffix, name } = profiles[environment];
const simulatorConfiguration = `Debug${suffix}`;
const archiveConfiguration = `Release${suffix}`;
const artifactPrefix = `${name}-1.0-${build}`;
const markers = [
  "OK Crew session visibility",
  "OK Crew attention transitions",
  "OK Crew Scaffold preparation",
  "OK Crew Scaffold first command retains originating generation across upload route refresh",
  "OK Crew APNs lifecycle",
  "OK Crew mobile parity",
  "OK Crew store eviction",
  "OK Crew peer message visibility",
  "OK Crew live list projection",
  "OK Crew room convergence",
];
const project = "apps/ios/Comet.xcodeproj";
const lockfile = path.join(project, "project.xcworkspace/xcshareddata/swiftpm/Package.resolved");
const lockBefore = readFileSync(lockfile);
let simulator;
let simulatorLog;

function requireEqual(actual, expected, label) {
  if (actual !== expected) throw new Error(`${label}: got ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
}

function run(command, args, { timeout = 120_000, log, allowFailure = false } = {}) {
  console.log(`+ ${command} ${args.join(" ")}`);
  const fd = log ? openSync(path.join(logs, log), "a") : undefined;
  let result;
  try {
    result = spawnSync(command, args, {
      cwd: root, env: process.env, encoding: "utf8", timeout, killSignal: "SIGKILL",
      maxBuffer: 16 * 1024 * 1024,
      stdio: fd === undefined ? ["ignore", "pipe", "pipe"] : ["ignore", fd, fd],
    });
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
  if (!allowFailure && (result.error || result.status !== 0)) {
    throw new Error(`${command} failed (${result.status ?? result.signal}): ${result.error?.message ?? result.stderr ?? ""}${log ? `; see ${log}` : ""}`);
  }
  return (result.stdout ?? "").trim();
}

function plist(file) {
  return JSON.parse(run("plutil", ["-convert", "json", "-o", "-", file]));
}

function descendingVersion(a, b) {
  return b.localeCompare(a, "en", { numeric: true });
}

function verifyApp(app, platform) {
  const info = plist(path.join(app, "Info.plist"));
  requireEqual(info.CFBundleIdentifier, bundleId, "Bundle identifier");
  requireEqual(info.CFBundleShortVersionString, "1.0", "Marketing version");
  requireEqual(info.CFBundleVersion, build, "Source build number (never overridden by CI)");
  requireEqual(info.DTPlatformName, platform, "Built platform");
  const hostSuffix = environment === "staging" ? "-staging" : "";
  requireEqual(info.CrewEdgeURL, `https://comet${hostSuffix}.internal.ashler.com`, "Edge endpoint");
  requireEqual(info.CrewScaffoldURL, `https://scaffold${hostSuffix}.internal.ashler.com`, "Scaffold endpoint");
  requireEqual(info.CrewProjectScope, `ashler-${environment}`, "Project scope");
  requireEqual(info.CrewInviteScheme, `comet${hostSuffix}`, "Invite scheme");
  requireEqual(run("lipo", ["-archs", path.join(app, info.CFBundleExecutable)]), "arm64", "Binary architecture");
  return info;
}

function snapshotE2ELog() {
  if (simulatorLog && existsSync(simulatorLog)) {
    copyFileSync(simulatorLog, path.join(logs, "e2e.log"));
    return readFileSync(simulatorLog, "utf8");
  }
  return "";
}

async function verifySimulator(app) {
  const runtimes = JSON.parse(run("xcrun", ["simctl", "list", "runtimes", "available", "--json"])).runtimes;
  const runtime = runtimes.filter((item) => item.isAvailable && item.identifier.includes(".iOS-") && /^26(?:\.|$)/.test(item.version))
    .sort((a, b) => descendingVersion(a.version, b.version))[0];
  if (!runtime) throw new Error("Runner has no available iOS 26 simulator runtime");
  const devices = JSON.parse(run("xcrun", ["simctl", "list", "devices", "available", "--json"])).devices;
  const template = devices[runtime.identifier]?.find((item) => item.isAvailable && item.name.startsWith("iPhone"));
  const types = JSON.parse(run("xcrun", ["simctl", "list", "devicetypes", "--json"])).devicetypes;
  const deviceType = template?.deviceTypeIdentifier ?? types.find((item) => item.name === template?.name)?.identifier;
  if (!deviceType) throw new Error(`No compatible installed iPhone template for ${runtime.identifier}`);
  simulator = run("xcrun", ["simctl", "create", `Crew CI ${process.env.GITHUB_RUN_ID}`, deviceType, runtime.identifier]);
  run("xcrun", ["simctl", "boot", simulator]);
  run("xcrun", ["simctl", "bootstatus", simulator, "-b"], { timeout: 300_000, log: "simulator.log" });
  run("xcrun", ["simctl", "install", simulator, app]);
  const container = run("xcrun", ["simctl", "get_app_container", simulator, bundleId, "data"]);
  simulatorLog = path.join(container, "Documents", "e2e.log");
  // A newly created simulator has no prior auth, preferences, or stale markers.
  // This hook enters demo mode; its APNs probe injects URLProtocol and never
  // requests notification authorization, registers with APNs, or uses live auth.
  run("xcrun", ["simctl", "launch", "--terminate-running-process", simulator, bundleId, "-visibility-e2e"], { log: "simulator.log" });
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    const text = snapshotE2ELog();
    if (/\bFAIL\b/.test(text)) throw new Error(`Mobile regression failed:\n${text}`);
    if (markers.every((marker) => text.split("\n").some((line) => new RegExp(`^\\[\\d+\\] ${marker}(?=[:\\s]|$)`).test(line)))) {
      console.log(text);
      return { identifier: runtime.identifier, version: runtime.version, deviceType, markers };
    }
    await sleep(1_000);
  }
  throw new Error(`Mobile regression timed out waiting for all ${markers.length} markers; see e2e.log:\n${snapshotE2ELog()}`);
}

function signArchiveApp(app, settings) {
  const source = readFileSync(path.resolve(settings.SRCROOT, settings.CODE_SIGN_ENTITLEMENTS), "utf8");
  const expanded = source.replace(/\$\(([^)]+)\)|\$\{([^}]+)\}/g, (_, parenthesized, braced) => {
    const key = parenthesized ?? braced;
    if (!(key in settings)) throw new Error(`Unresolved entitlement build setting: ${key}`);
    return String(settings[key]).replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
  });
  const entitlements = path.join(logs, "archive-expanded.entitlements");
  writeFileSync(entitlements, expanded);
  requireEqual(plist(entitlements)["aps-environment"], "production", "Expanded archive push entitlement");
  // Sign embedded frameworks inside-out first, without app-only entitlements.
  // The final signature is ad-hoc, not an Apple distribution signature.
  const embedded = [];
  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) visit(file);
      if ((entry.isDirectory() && entry.name.endsWith(".framework")) || (entry.isFile() && entry.name.endsWith(".dylib"))) embedded.push(file);
    }
  }
  visit(app);
  for (const file of embedded) run("codesign", ["--force", "--sign", "-", "--timestamp=none", file], { log: "codesign.log" });
  run("codesign", ["--force", "--sign", "-", "--timestamp=none", "--entitlements", entitlements, app], { log: "codesign.log" });
  run("codesign", ["--verify", "--deep", "--strict", app], { log: "codesign.log" });
  const actual = path.join(logs, "archive-signed.entitlements");
  writeFileSync(actual, run("codesign", ["--display", "--entitlements", ":-", app]));
  requireEqual(plist(actual)["aps-environment"], "production", "Embedded signature push entitlement");
  if (existsSync(path.join(app, "embedded.mobileprovision"))) throw new Error("Archive unexpectedly contains a provisioning profile");
}

async function sha256(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

try {
  requireEqual(run("git", ["rev-parse", "HEAD"]), process.env.GITHUB_SHA, "Checkout source SHA");
  const xcodes = readdirSync("/Applications").filter((name) => /^Xcode_26(?:\.\d+)*\.app$/.test(name))
    .sort(descendingVersion);
  if (!xcodes.length) throw new Error("Runner has no stable Xcode 26 installation");
  process.env.DEVELOPER_DIR = path.join(realpathSync(path.join("/Applications", xcodes[0])), "Contents/Developer");
  const xcode = run("xcodebuild", ["-version"]);
  if (!/^Xcode 26(?:\.|\s)/.test(xcode)) throw new Error(`Expected Xcode 26: ${xcode}`);
  const sdk = run("xcrun", ["--sdk", "iphoneos", "--show-sdk-version"]);
  if (!/^26(?:\.|$)/.test(sdk)) throw new Error(`Expected iOS 26 SDK: ${sdk}`);
  console.log(`${process.env.DEVELOPER_DIR}\n${xcode}\niOS SDK ${sdk}`);
  const base = [
    "-project", project, "-scheme", scheme, "-jobs", "2",
    "-clonedSourcePackagesDirPath", path.join(work, "packages"),
    "-disableAutomaticPackageResolution", "-onlyUsePackageVersionsFromResolvedFile",
  ];
  const simulatorBuild = [
    ...base, "-configuration", simulatorConfiguration, "-sdk", "iphonesimulator",
    "-destination", "generic/platform=iOS Simulator", "-derivedDataPath", path.join(work, "simulator-derived"),
    "ARCHS=arm64", "ONLY_ACTIVE_ARCH=YES", "CODE_SIGNING_ALLOWED=YES", "CODE_SIGN_IDENTITY=-",
  ];
  const archive = path.join(work, `${name}.xcarchive`);
  const deviceBuild = [
    ...base, "-configuration", archiveConfiguration, "-sdk", "iphoneos",
    "-destination", "generic/platform=iOS", "-derivedDataPath", path.join(work, "device-derived"),
    "ARCHS=arm64", "CODE_SIGNING_ALLOWED=NO",
  ];
  const buildSettings = {};
  for (const [name, args] of [["simulator", simulatorBuild], ["archive", deviceBuild]]) {
    const json = run("xcodebuild", [...args, "-showBuildSettings", "-json"], { timeout: 600_000 });
    writeFileSync(path.join(logs, `${name}-build-settings.json`), json);
    const settings = JSON.parse(json).find((item) => item.target === "Comet")?.buildSettings;
    if (!settings) throw new Error(`Missing Comet build settings for ${name}`);
    requireEqual(settings.CURRENT_PROJECT_VERSION, build, `${name} source build number`);
    requireEqual(settings.MARKETING_VERSION, "1.0", `${name} source marketing version`);
    requireEqual(settings.PRODUCT_BUNDLE_IDENTIFIER, bundleId, `${name} source bundle identifier`);
    buildSettings[name] = settings;
  }
  run("xcodebuild", [...simulatorBuild, "build"], { timeout: 1_200_000, log: "simulator-build.log" });
  const simulatorApp = path.join(buildSettings.simulator.TARGET_BUILD_DIR, buildSettings.simulator.FULL_PRODUCT_NAME);
  const simulatorInfo = verifyApp(simulatorApp, "iphonesimulator");
  const regression = await verifySimulator(simulatorApp);
  run("xcodebuild", [...deviceBuild, "-archivePath", archive, "archive"], { timeout: 1_200_000, log: "archive-build.log" });
  const applicationPath = run("plutil", ["-extract", "ApplicationProperties.ApplicationPath", "raw", "-o", "-", path.join(archive, "Info.plist")]);
  const archiveApp = path.join(archive, "Products", applicationPath);
  const deviceInfo = verifyApp(archiveApp, "iphoneos");
  signArchiveApp(archiveApp, buildSettings.archive);
  if (!lockBefore.equals(readFileSync(lockfile))) throw new Error("Build modified Package.resolved");

  // Upload-artifact does not preserve Unix modes; tar does, including symlinks.
  run("tar", ["-czf", path.join(release, `${artifactPrefix}-simulator-arm64.tar.gz`), "-C", path.dirname(simulatorApp), path.basename(simulatorApp)], { timeout: 300_000 });
  run("tar", ["-czf", path.join(release, `${artifactPrefix}-unsigned.xcarchive.tar.gz`), "-C", work, path.basename(archive)], { timeout: 300_000 });
  copyFileSync(path.join(logs, "e2e.log"), path.join(release, "e2e.log"));
  copyFileSync(path.join(logs, "archive-signed.entitlements"), path.join(release, "archive-signed.entitlements"));
  writeFileSync(path.join(release, "source-sha.txt"), `${process.env.GITHUB_SHA}\n`);
  writeFileSync(path.join(release, "provenance.json"), `${JSON.stringify({
    source: { repository: process.env.GITHUB_REPOSITORY, sha: process.env.GITHUB_SHA, ref: process.env.GITHUB_REF },
    workflow: { runId: process.env.GITHUB_RUN_ID, attempt: process.env.GITHUB_RUN_ATTEMPT, url: `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}` },
    runner: { architecture: process.arch, image: process.env.ImageOS, imageVersion: process.env.ImageVersion },
    toolchain: { developerDirectory: process.env.DEVELOPER_DIR, xcode, sdk },
    bundleId, version: deviceInfo.CFBundleShortVersionString, build: deviceInfo.CFBundleVersion,
    simulator: { configuration: simulatorConfiguration, platform: simulatorInfo.DTPlatformName, ...regression },
    archive: { configuration: archiveConfiguration, path: path.basename(archive), signing: "ad-hoc; requires local Apple distribution re-sign/export", apsEnvironment: "production" },
    packageResolvedSha256: await sha256(lockfile),
  }, null, 2)}\n`);
  const checksums = [];
  for (const name of readdirSync(release).sort()) checksums.push(`${await sha256(path.join(release, name))}  ${name}`);
  writeFileSync(path.join(release, "SHA256SUMS"), `${checksums.join("\n")}\n`);
  console.log(`Verified ${name} 1.0 (${build}); release files: ${release}`);
} catch (error) {
  console.error(error.stack ?? error);
  writeFileSync(path.join(logs, "failure.log"), `${error.stack ?? error}\n`);
  process.exitCode = 1;
} finally {
  snapshotE2ELog();
  if (simulator) {
    run("xcrun", ["simctl", "shutdown", simulator], { allowFailure: true, timeout: 30_000 });
    run("xcrun", ["simctl", "delete", simulator], { allowFailure: true, timeout: 30_000 });
  }
}
