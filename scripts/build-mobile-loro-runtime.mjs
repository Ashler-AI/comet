// Usage: node scripts/build-mobile-loro-runtime.mjs PACKAGES_DIRECTORY SCHEME
// Keep the shipped Swift/UniFFI ABI; use this workspace's patched Loro core.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

assert.equal(process.argv.length, 4,
  "Usage: node scripts/build-mobile-loro-runtime.mjs PACKAGES_DIRECTORY SCHEME");
const packages = resolve(process.argv[2]);
const scheme = process.argv[3];
const project = "apps/ios/Comet.xcodeproj";
const sdk = join(packages, "checkouts/loro-swift");
const artifact = join(packages, "artifacts/loro-swift/LoroFFI");
const env = { ...process.env, ASHLER_INCREMENTAL_TSC_CHECKS: "false" };
delete env.LOCAL_BUILD;
const run = (command, args, options = {}) => execFileSync(command, args, { env, stdio: "inherit", ...options });
const text = (command, args) => run(command, args, { encoding: "utf8", stdio: "pipe" }).trim();
const digest = file => createHash("sha256").update(readFileSync(file)).digest("hex");
const graph = ["-resolvePackageDependencies", "-project", project, "-scheme", scheme,
  "-clonedSourcePackagesDirPath", packages, "-disableAutomaticPackageResolution", "-onlyUsePackageVersionsFromResolvedFile"];
run("xcodebuild", graph);
const sdkRevision = text("git", ["-C", sdk, "rev-parse", "HEAD"]);
assert.equal(sdkRevision, "625f3e696fca4be3ae77de8b3404fa6753554f21", "Unexpected Swift FFI ABI source");
const info = path => JSON.parse(text("plutil", ["-convert", "json", "-o", "-", path]));
const originalFramework = join(artifact, "loroFFI.xcframework");
const device = info(join(originalFramework, "Info.plist")).AvailableLibraries.find(
  library => library.SupportedPlatform === "ios" && !library.SupportedPlatformVariant);
assert.ok(device?.HeadersPath, "Pinned SDK has no iOS headers");
const work = mkdtempSync(join(tmpdir(), "crew-mobile-loro-"));
try {
  const headers = join(work, "Headers");
  cpSync(join(originalFramework, device.LibraryIdentifier, device.HeadersPath), headers, { recursive: true });
  // rustup can be installed without a cargo shim on a developer workstation.
  env.PATH = dirname(text("rustup", ["which", "cargo"])) + ":" + env.PATH;
  const targets = ["aarch64-apple-ios", "aarch64-apple-ios-sim"];
  run("rustup", ["target", "add", ...targets]);
  const libraries = [];
  for (const target of targets) {
    const directory = join(work, target);
    mkdirSync(directory);
    const library = join(directory, "libloro_swift.a");
    run("python3", ["scripts/local-cargo.py", "build", "--locked", "--release", "-p", "comet-mobile-ffi",
      "--target", target, "--copy-binary", `${target}/release/libloro_swift.a`, library]);
    libraries.push({ target, library, sha256: digest(library) });
  }
  const framework = join(work, "loroFFI.xcframework");
  run("xcodebuild", ["-create-xcframework", ...libraries.flatMap(({ library }) => ["-library", library, "-headers", headers]),
    "-output", framework]);
  const archive = join(sdk, "loroFFI.xcframework.zip");
  run("ditto", ["-c", "-k", "--sequesterRsrc", "--keepParent", framework, archive]);
  // The SDK already supports a local binary through LOCAL_BUILD. Remove only
  // this resolved SDK artifact so the graph cannot silently retain its old core.
  rmSync(artifact, { recursive: true });
  env.LOCAL_BUILD = "1";
  run("xcodebuild", graph);
  const resolved = join(artifact, "loroFFI.xcframework");
  const actual = info(join(resolved, "Info.plist")).AvailableLibraries;
  for (const expected of libraries) {
    const simulator = expected.target.endsWith("-sim");
    const library = actual.find(item => item.SupportedPlatform === "ios"
      && (item.SupportedPlatformVariant === "simulator") === simulator);
    assert.ok(library, `Missing rebuilt ${expected.target} slice`);
    assert.equal(digest(join(resolved, library.LibraryIdentifier, library.LibraryPath)), expected.sha256,
      `${expected.target}: SwiftPM did not select the rebuilt native core`);
  }
  const receipt = {
    swiftSdk: { version: "1.13.3", revision: sdkRevision },
    ffi: "1.13.7", uniffi: "0.31.1", nativeCore: "1.13.9",
    cargoLockSha256: digest("Cargo.lock"), archiveSha256: digest(archive),
    libraries: libraries.map(({ target, sha256 }) => ({ target, sha256 })),
  };
  writeFileSync(join(packages, "crew-mobile-loro-runtime.json"), JSON.stringify(receipt, null, 2) + "\n");
  console.log("Crew mobile native runtime verified: unchanged Swift ABI, workspace Loro core");
} finally { rmSync(work, { recursive: true, force: true }); }
