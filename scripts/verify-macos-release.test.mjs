import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const env = { ...process.env, ASHLER_INCREMENTAL_TSC_CHECKS: "false", COMET_MACOS_SIGNING_TEAM_ID: "TESTTEAM01" };
function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, env, encoding: "utf8", timeout: 30_000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  return result;
}

for (const [bundleId, rejection] of [
  ["ai.ashler.comet", /codesign|requirement|Requirement|signature/],
  ["ai.ashler.comet.staging", /bundle identifier mismatch/],
]) {
test(`valid release checksums cannot authorize an untrusted ${bundleId} update`, { skip: process.platform !== "darwin" }, () => {
  const temp = mkdtempSync(path.join(tmpdir(), "crew-candidate-trust-"));
  try {
    const app = path.join(temp, "Crew.app");
    const macos = path.join(app, "Contents", "MacOS");
    mkdirSync(macos, { recursive: true });
    copyFileSync("/usr/bin/true", path.join(macos, "comet"));
    copyFileSync("/usr/bin/true", path.join(macos, "Crew Engine"));
    run("/usr/bin/codesign", ["--force", "--sign", "-", path.join(macos, "Crew Engine")], temp);
    writeFileSync(path.join(app, "Contents", "Info.plist"), `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>${bundleId}</string>
<key>CFBundleExecutable</key><string>comet</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleVersion</key><string>1.2.3</string>
<key>CFBundleShortVersionString</key><string>1.2.3</string>
</dict></plist>`);
    run("/usr/bin/codesign", ["--force", "--sign", "-", app], temp);
    run("/usr/bin/codesign", ["--verify", "--deep", "--strict", app], temp);
    const archive = "comet-1.2.3-macos-arm64-app.tar.gz";
    const dmg = "comet-1.2.3-macos-arm64.dmg";
    run("/usr/bin/tar", ["-czf", path.join(temp, archive), "-C", temp, "Crew.app"], temp);
    // Rejection must happen at app trust verification, before attempting a mount.
    writeFileSync(path.join(temp, dmg), "not mounted: untrusted app must be rejected first");
    writeFileSync(path.join(temp, "desktop-SHA256SUMS"), [archive, dmg].map(name =>
      `${createHash("sha256").update(readFileSync(path.join(temp, name))).digest("hex")}  ${name}\n`
    ).join(""));
    run("/usr/bin/shasum", ["-a", "256", "--check", "desktop-SHA256SUMS"], temp);
    const result = spawnSync("/bin/bash", [path.join(root, "scripts", "verify-macos-release.sh"), temp, "1.2.3"], {
      cwd: root, env, encoding: "utf8", timeout: 30_000,
    });
    assert.ifError(result.error);
    assert.notEqual(result.status, null);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, rejection);
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
});
}

for (const defect of ["missing-dmg", "missing-archive", "duplicate", "malformed"]) {
  test(`release verification rejects ${defect} artifact checksum entries before extraction`, () => {
    const temp = mkdtempSync(path.join(tmpdir(), "crew-candidate-checksums-"));
    try {
      const archive = "comet-1.2.3-macos-arm64-app.tar.gz";
      const dmg = "comet-1.2.3-macos-arm64.dmg";
      const entries = [archive, dmg].map(name => {
        writeFileSync(path.join(temp, name), "must not be extracted or mounted");
        return `${createHash("sha256").update(readFileSync(path.join(temp, name))).digest("hex")}  ${name}\n`;
      });
      if (defect === "missing-dmg") entries.pop();
      if (defect === "missing-archive") entries.shift();
      if (defect === "duplicate") entries.push(entries[0]);
      if (defect === "malformed") entries[0] = `invalid  ${archive}\n`;
      writeFileSync(path.join(temp, "desktop-SHA256SUMS"), entries.join(""));
      const result = spawnSync("/bin/bash", [path.join(root, "scripts", "verify-macos-release.sh"), temp, "1.2.3"], {
        env, encoding: "utf8", timeout: 10_000,
      });
      assert.ifError(result.error);
      assert.equal(result.status, 1, result.stderr);
      assert.match(result.stderr, /Expected one SHA-256 entry/);
    } finally {
      rmSync(temp, { recursive: true, force: true });
    }
  });
}
