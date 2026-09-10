import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const nativeHost = process.platform === "darwin" && process.arch === "arm64";

// Stop at the build boundary: these tests must never compile, sign, or notarize.
function probe(overrides, certificateTeam) {
  const bin = mkdtempSync(path.join(tmpdir(), "crew-package-preflight-"));
  try {
    const cargo = path.join(bin, "cargo");
    writeFileSync(cargo, '#!/bin/sh\nprintf "BUILD_BOUNDARY %s %s\\n" "$COMET_PACKAGE_ENVIRONMENT" "$COMET_DEFAULT_ENVIRONMENT"\nexit 86\n');
    chmodSync(cargo, 0o700);
    const xcrun = path.join(bin, "xcrun");
    writeFileSync(xcrun, '#!/bin/sh\nprintf "NOTARY_BOUNDARY\\n"\nexit 87\n');
    chmodSync(xcrun, 0o700);
    if (certificateTeam) {
      // A disposable self-signed certificate models keychain inventory only;
      // no real signing identity, keychain mutation, or notarization is used.
      const certificate = path.join(bin, "identity.pem");
      const generated = spawnSync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        "-subj", `/CN=Developer ID Application: Test (${certificateTeam})/OU=${certificateTeam}`,
        "-keyout", path.join(bin, "identity.key"), "-out", certificate], { encoding: "utf8" });
      assert.equal(generated.status, 0, generated.stderr);
      const fingerprint = spawnSync("openssl", ["x509", "-in", certificate, "-noout", "-fingerprint", "-sha1"], { encoding: "utf8" });
      assert.equal(fingerprint.status, 0, fingerprint.stderr);
      const hash = fingerprint.stdout.trim().split("=")[1].replaceAll(":", "");
      const security = path.join(bin, "security");
      writeFileSync(security, `#!/bin/sh\ncase "$1" in\nfind-identity) printf '%s\\n' '  1) ${hash} "Developer ID Application: Test (${certificateTeam})"' ;;\nfind-certificate) printf '%s\\n' '${readFileSync(certificate, "utf8")}' ;;\n*) exit 90 ;;\nesac\n`);
      chmodSync(security, 0o700);
      overrides = { ...overrides, CODESIGN_IDENTITY: hash };
    }
    const env = { ...process.env };
    for (const key of [
      "CI", "CODESIGN_IDENTITY", "CODESIGN_KEYCHAIN", "NOTARYTOOL_KEYCHAIN_PROFILE",
      "COMET_MACOS_SIGNING", "COMET_PACKAGE_ENVIRONMENT", "COMET_DEFAULT_ENVIRONMENT",
      "COMET_RELEASE_VERSION", "COMET_MACOS_SIGNING_TEAM_ID",
    ]) delete env[key];
    // macOS can launch /bin/bash translated even from an arm64 Node process.
    // Exercise the packaging policy, not the earlier host-architecture guard.
    return spawnSync("/usr/bin/arch", ["-arm64", "/bin/bash", "scripts/package-macos.sh"], {
      cwd: root,
      env: { ...env, PATH: `${bin}${path.delimiter}${env.PATH}`, ASHLER_INCREMENTAL_TSC_CHECKS: "false", ...overrides },
      encoding: "utf8",
      timeout: 10_000,
    });
  } finally {
    rmSync(bin, { recursive: true, force: true });
  }
}

function rejectedBeforeBuild(result) {
  assert.ifError(result.error);
  assert.equal(result.status, 1);
  assert.doesNotMatch(result.stdout, /BUILD_BOUNDARY/);
  assert.doesNotMatch(result.stdout, /NOTARY_BOUNDARY/);
}

test("distribution cannot compile without explicit signing and notarization credentials", { skip: !nativeHost }, () => {
  rejectedBeforeBuild(probe({}));
});

test("distribution requires an independent well-formed signing team before any notarization", { skip: !nativeHost }, () => {
  const credentials = { CODESIGN_IDENTITY: "Developer ID Application: Test (ABCDEFGHIJ)", NOTARYTOOL_KEYCHAIN_PROFILE: "unused" };
  rejectedBeforeBuild(probe(credentials));
  rejectedBeforeBuild(probe({ ...credentials, COMET_MACOS_SIGNING_TEAM_ID: "abcde12345" }));
});

test("a selected certificate from another team is rejected before notarization or compilation", { skip: !nativeHost }, () => {
  rejectedBeforeBuild(probe({ COMET_MACOS_SIGNING_TEAM_ID: "ABCDEFGHIJ", NOTARYTOOL_KEYCHAIN_PROFILE: "unused" }, "ZZZZZZZZZZ"));
});

test("a matching certificate team reaches notarization preflight without compiling", { skip: !nativeHost }, () => {
  const result = probe({ COMET_MACOS_SIGNING_TEAM_ID: "ABCDEFGHIJ", NOTARYTOOL_KEYCHAIN_PROFILE: "unused" }, "ABCDEFGHIJ");
  assert.ifError(result.error);
  assert.equal(result.status, 87, result.stderr + result.stdout);
  assert.doesNotMatch(result.stdout, /BUILD_BOUNDARY/);
});

test("staging cannot package a production-default executable", { skip: !nativeHost }, () => {
  rejectedBeforeBuild(probe({ COMET_MACOS_SIGNING: "adhoc", COMET_PACKAGE_ENVIRONMENT: "staging", COMET_DEFAULT_ENVIRONMENT: "production" }));
});

test("ad-hoc packaging is forbidden in CI but accepts an explicitly local CI=false session", { skip: !nativeHost }, () => {
  rejectedBeforeBuild(probe({ CI: "true", COMET_MACOS_SIGNING: "adhoc" }));
  const local = probe({ CI: "false", COMET_MACOS_SIGNING: "adhoc", COMET_PACKAGE_ENVIRONMENT: "staging" });
  assert.ifError(local.error);
  assert.equal(local.status, 86, local.stderr + local.stdout);
  assert.match(local.stdout, /BUILD_BOUNDARY staging staging/);
});

test("distribution verification rejects wrong bundle metadata and unsigned executables", { skip: !nativeHost }, () => {
  const directory = mkdtempSync(path.join(tmpdir(), "crew-verify-app-"));
  const app = path.join(directory, "Crew.app");
  try {
    mkdirSync(path.join(app, "Contents", "MacOS"), { recursive: true });
    const executable = path.join(app, "Contents", "MacOS", "comet");
    writeFileSync(executable, "#!/bin/sh\nexit 0\n");
    chmodSync(executable, 0o700);
    const verify = (identifier) => {
      writeFileSync(path.join(app, "Contents", "Info.plist"), `<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>${identifier}</string><key>CFBundleExecutable</key><string>comet</string></dict></plist>`);
      return spawnSync("/bin/bash", ["scripts/verify-macos-app.sh", app, "ai.ashler.comet"], {
        cwd: root,
        env: { ...process.env, COMET_MACOS_SIGNING_TEAM_ID: "ABCDEFGHIJ", ASHLER_INCREMENTAL_TSC_CHECKS: "false" },
        encoding: "utf8", timeout: 10_000,
      });
    };
    for (const identifier of ["ai.ashler.comet.staging", "ai.ashler.comet"]) {
      const result = verify(identifier);
      assert.ifError(result.error);
      assert.notEqual(result.status, 0);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
