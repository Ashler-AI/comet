import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";
import { validateReleaseCandidateReuse } from "./validate-release-candidate-reuse.mjs";
import { releaseFeedSecrets, syncReleaseFeedSecrets } from "./sync-edge-release-feed-secrets.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (relative) => readFile(path.join(root, relative), "utf8");

const SHA256 = "a".repeat(64);
const RUN_SHA = "b".repeat(40);
const RUN_ID = "32313877342";
const REPOSITORY = "Ashler-AI/comet";
const VERSION = "0.1.57";
const RUN_URL = `https://github.com/${REPOSITORY}/actions/runs/${RUN_ID}`;

function releaseManifest(releaseSurface, files, schemaVersion = 2) {
  return {
    schemaVersion,
    ...(releaseSurface ? { releaseSurface } : {}),
    ...(schemaVersion === 2 ? { scaffoldRuntimeVersion: "scaffold.comet-runtime.v1" } : {}),
    version: VERSION,
    source: {
      repository: REPOSITORY,
      commit: RUN_SHA,
      workflowRun: RUN_URL,
    },
    files: Object.fromEntries(files.map((name) => [name, { sha256: SHA256 }])),
  };
}

function reusableCandidate(overrides = {}) {
  return {
    run: {
      id: Number(RUN_ID),
      repository: { full_name: REPOSITORY },
      head_repository: { full_name: REPOSITORY },
      path: ".github/workflows/release.yml",
      event: "workflow_dispatch",
      status: "completed",
      conclusion: "success",
      head_branch: "main",
      head_sha: RUN_SHA,
      html_url: RUN_URL,
    },
    desktopManifest: releaseManifest("desktop", [
      `comet-${VERSION}-macos-arm64.dmg`,
      `comet-${VERSION}-macos-arm64-app.tar.gz`,
    ]),
    desktopStagingManifest: releaseManifest("desktop-staging", [
      `comet-staging-${VERSION}-macos-arm64.dmg`,
      `comet-staging-${VERSION}-macos-arm64-app.tar.gz`,
    ]),
    scaffoldManifest: releaseManifest("scaffold", [
      `comet-${VERSION}-linux-aarch64.tar.gz`,
      `comet-${VERSION}-linux-x86_64.tar.gz`,
    ]),
    unifiedManifest: releaseManifest(undefined, [
      `comet-${VERSION}-linux-aarch64.tar.gz`,
      `comet-${VERSION}-linux-x86_64.tar.gz`,
      `comet-${VERSION}-macos-arm64.dmg`,
      `comet-${VERSION}-macos-arm64-app.tar.gz`,
    ], 1),
    requestedVersion: VERSION,
    releaseSurface: "desktop-and-scaffold",
    repository: REPOSITORY,
    runId: RUN_ID,
    ...overrides,
  };
}

function jobBlock(workflow, name) {
  const marker = `  ${name}:\n`;
  const start = workflow.indexOf(marker);
  assert.notEqual(start, -1, `missing job ${name}`);
  const next = workflow.slice(start + marker.length).search(/\n  [A-Za-z0-9_-]+:\n/);
  return workflow.slice(start, next === -1 ? undefined : start + marker.length + next);
}

function shellStep(workflow, job, id) {
  const block = jobBlock(workflow.slice(workflow.indexOf("\njobs:\n")), job);
  const start = block.indexOf(`      - id: ${id}\n`);
  assert.notEqual(start, -1, `missing step ${job}.${id}`);
  const rest = block.slice(start);
  const next = rest.indexOf("\n      - ");
  const step = next === -1 ? rest : rest.slice(0, next);
  const script = step.match(/        run: \|\n([\s\S]*)$/)?.[1];
  assert.ok(script, `missing shell in ${job}.${id}`);
  return script.replace(/^          /gm, "");
}

const fixtureEnv = {
  ...process.env,
  ASHLER_INCREMENTAL_TSC_CHECKS: "false",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_COUNT: "1",
  GIT_CONFIG_KEY_0: "core.hooksPath",
  GIT_CONFIG_VALUE_0: "/dev/null",
  GIT_AUTHOR_NAME: "Crew release test",
  GIT_AUTHOR_EMAIL: "release-test@example.invalid",
  GIT_COMMITTER_NAME: "Crew release test",
  GIT_COMMITTER_EMAIL: "release-test@example.invalid",
};

function command(cwd, executable, args, env = {}) {
  const result = spawnSync(executable, args, {
    cwd, env: { ...fixtureEnv, ...env }, encoding: "utf8", timeout: 30_000,
  });
  assert.ifError(result.error);
  return result;
}

function fixtureCommand(cwd, executable, ...args) {
  const result = command(cwd, executable, args);
  assert.equal(result.status, 0, result.stderr);
  return result.stdout.trim();
}

function scratch(t) {
  const dir = mkdtempSync(path.join(tmpdir(), "crew-release-gate-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function releaseCheckout(t, merged) {
  const dir = scratch(t);
  const upstream = path.join(dir, "upstream");
  const checkout = path.join(dir, "checkout");
  fixtureCommand(dir, "git", "init", "-b", "main", upstream);
  writeFileSync(path.join(upstream, "Cargo.toml"), `[workspace]\nmembers = ["."]\n[workspace.package]\nversion = "${VERSION}"\n[package]\nname = "zeron"\nversion.workspace = true\nedition = "2024"\n[lib]\npath = "lib.rs"\n`);
  writeFileSync(path.join(upstream, "lib.rs"), "");
  writeFileSync(path.join(upstream, "Cargo.lock"), `version = 4\n\n[[package]]\nname = "zeron"\nversion = "${VERSION}"\n`);
  fixtureCommand(upstream, "git", "add", ".");
  fixtureCommand(upstream, "git", "commit", "-m", "main baseline");
  fixtureCommand(upstream, "git", "checkout", "-b", "release-fixture");
  writeFileSync(path.join(upstream, "release-input"), "candidate\n");
  fixtureCommand(upstream, "git", "add", ".");
  fixtureCommand(upstream, "git", "commit", "-m", "candidate");
  const sha = fixtureCommand(upstream, "git", "rev-parse", "HEAD");
  fixtureCommand(upstream, "git", "checkout", "main");
  if (merged) fixtureCommand(upstream, "git", "merge", "--ff-only", "release-fixture");
  fixtureCommand(dir, "git", "clone", "--single-branch", "--branch", "release-fixture", "--no-local", upstream, checkout);
  assert.notEqual(command(checkout, "git", ["rev-parse", "--verify", "refs/remotes/origin/main"]).status, 0);
  return { checkout, sha };
}

function runVersion(script, { checkout, sha }, overrides = {}) {
  const output = path.join(checkout, "version-output");
  writeFileSync(output, "");
  const result = command(checkout, "bash", ["-c", script], {
    GITHUB_REF: "refs/heads/release-fixture", GITHUB_SHA: sha, GITHUB_OUTPUT: output,
    REQUESTED_VERSION: VERSION, REQUESTED_RELEASE_SURFACE: "desktop",
    REQUESTED_PROMOTION_TARGET: "production", REQUESTED_CANDIDATE_RUN_ID: RUN_ID,
    ...overrides,
  });
  return { ...result, output: readFileSync(output, "utf8") };
}

const digest = (data) => createHash("sha256").update(data).digest("hex");

function candidateArchive(t, { corrupt, missingStaging = false, archiveCorrupt = false, branch = "release-fixture" } = {}) {
  const dir = scratch(t);
  const payload = path.join(dir, "payload");
  for (const name of ["payload", "reused", "scripts", "bin"]) mkdirSync(path.join(dir, name));
  copyFileSync(path.join(root, "scripts/validate-release-candidate-reuse.mjs"), path.join(dir, "scripts/validate-release-candidate-reuse.mjs"));
  const candidate = reusableCandidate();
  candidate.run.head_branch = branch;
  writeFileSync(path.join(dir, "candidate-source-run.json"), JSON.stringify(candidate.run));
  for (const name of [...Object.keys(candidate.unifiedManifest.files), ...Object.keys(candidate.desktopStagingManifest.files)]) {
    writeFileSync(path.join(payload, name), `release artifact ${name}\n`);
  }
  for (const [manifestName, sumsName, manifest] of [
    ["desktop-manifest.json", "desktop-SHA256SUMS", candidate.desktopManifest],
    ["desktop-staging-manifest.json", "desktop-staging-SHA256SUMS", candidate.desktopStagingManifest],
    ["scaffold-manifest.json", "scaffold-SHA256SUMS", candidate.scaffoldManifest],
    ["manifest.json", "SHA256SUMS", candidate.unifiedManifest],
  ]) {
    const sums = Object.keys(manifest.files).map((name) => {
      const sha = digest(readFileSync(path.join(payload, name)));
      manifest.files[name].sha256 = sha;
      return `${corrupt === sumsName ? "0".repeat(64) : sha}  ${name}\n`;
    }).join("");
    writeFileSync(path.join(payload, manifestName), JSON.stringify(manifest));
    writeFileSync(path.join(payload, sumsName), sums);
  }
  writeFileSync(path.join(payload, "install.sh"), "#!/bin/sh\n");
  writeFileSync(path.join(payload, "COMET_SHA256SUMS"), Object.keys(candidate.scaffoldManifest.files)
    .map(name => `${SHA256}  ${name.replace(/\.tar\.gz$/, "/comet")}\n`).join(""));
  writeFileSync(path.join(payload, "provenance.json"), JSON.stringify({
    schemaVersion: 1, version: VERSION, sourceRevision: RUN_SHA,
    runtimeContract: "scaffold.comet-runtime.v1", repository: REPOSITORY,
    workflowRun: RUN_URL, delivery: "authenticated-private-gcs",
  }));
  if (missingStaging) rmSync(path.join(payload, "desktop-staging-manifest.json"));
  const archive = path.join(dir, "reused/release-candidate.tar.gz");
  fixtureCommand(dir, "tar", "-czf", archive, "-C", payload, ".");
  const sha = digest(readFileSync(archive));
  writeFileSync(`${archive}.sha256`, `${archiveCorrupt ? "0".repeat(64) : sha}  release-candidate.tar.gz\n`);
  // The GitHub compare endpoint is the external boundary; all local checksum,
  // manifest validation and publication-output logic below is the real step.
  writeFileSync(path.join(dir, "bin/gh"), '#!/bin/sh\n[ "$1" = api ] && [ "$2" = "repos/$GITHUB_REPOSITORY/compare/' + RUN_SHA + '...$GITHUB_SHA" ] || exit 2\nprintf "%s\\n" "$COMPARE_STATUS"\n', { mode: 0o755 });
  return { dir, sha };
}

function runReuse(script, { dir }, overrides = {}) {
  const output = path.join(dir, "reuse-output");
  writeFileSync(output, "");
  const result = command(dir, "bash", ["-c", script], {
    PATH: `${path.join(dir, "bin")}${path.delimiter}${process.env.PATH}`,
    GITHUB_REPOSITORY: REPOSITORY, GITHUB_SHA: "c".repeat(40), GITHUB_OUTPUT: output,
    VERSION, RELEASE_SURFACE: "desktop-and-scaffold", CANDIDATE_RUN_ID: RUN_ID,
    COMPARE_STATUS: "ahead", ...overrides,
  });
  return { ...result, output: readFileSync(output, "utf8") };
}

describe("executable release gates", () => {
  it("accepts a merged release checkout without an origin/main tracking ref", async (t) => {
    const checkout = releaseCheckout(t, true);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    const result = runVersion(script, checkout);
    assert.equal(result.status, 0, result.stderr);
    assert.ok(result.output.includes(`candidate_run_id=${RUN_ID}\n`));
    assert.notEqual(command(checkout.checkout, "git", ["rev-parse", "--verify", "refs/remotes/origin/main"]).status, 0);
  });

  it("rejects unmerged publication even when its exact source was reviewed", async (t) => {
    const checkout = releaseCheckout(t, false);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    for (const target of ["staging-candidate", "staging", "production"]) {
      const result = runVersion(script, checkout, {
        REQUESTED_PROMOTION_TARGET: target, REQUESTED_CANDIDATE_RUN_ID: "",
        REQUESTED_REVIEWED_SHA: checkout.sha,
      });
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
    }
  });

  it("builds unmerged private artifacts only for the exact reviewed SHA", async (t) => {
    const checkout = releaseCheckout(t, false);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    for (const reviewed of ["", "c".repeat(40), checkout.sha]) {
      const result = runVersion(script, checkout, {
        REQUESTED_PROMOTION_TARGET: "none", REQUESTED_CANDIDATE_RUN_ID: "",
        REQUESTED_REVIEWED_SHA: reviewed,
      });
      assert.equal(result.status === 0, reviewed === checkout.sha, result.stderr);
      if (reviewed === checkout.sha) assert.match(result.output, /^promotion_target=none$/m);
      else assert.equal(result.output, "");
    }
  });

  it("accepts desktop staging reuse of a full candidate without changing its archive digest", async (t) => {
    const workflow = await read(".github/workflows/release.yml");
    const version = runVersion(shellStep(workflow, "version", "version"), releaseCheckout(t, true), {
      REQUESTED_PROMOTION_TARGET: "staging",
    });
    assert.equal(version.status, 0, version.stderr);
    const outputs = Object.fromEntries(version.output.trim().split("\n").map((line) => line.split("=")));
    const fixture = candidateArchive(t);
    const reused = runReuse(shellStep(workflow, "candidate", "reuse"), fixture, {
      VERSION: outputs.version,
      RELEASE_SURFACE: outputs.release_surface,
      CANDIDATE_RUN_ID: outputs.candidate_run_id,
    });
    assert.equal(reused.status, 0, reused.stderr);
    assert.equal(reused.output, `digest=${fixture.sha}\n`);
    assert.equal(digest(readFileSync(path.join(fixture.dir, "release-candidate.tar.gz"))), fixture.sha);
  });

  it("rejects invalid reuse identifiers and dispatches without channel promotion", async (t) => {
    const checkout = releaseCheckout(t, true);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    for (const overrides of [
      { REQUESTED_CANDIDATE_RUN_ID: "not-a-run-id" },
      { REQUESTED_PROMOTION_TARGET: "none" },
      { REQUESTED_PROMOTION_TARGET: "staging-candidate" },
    ]) {
      const result = runVersion(script, checkout, overrides);
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
    }
  });

  it("reuses verified bytes only when GitHub reports ancestor or identical source", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
    for (const status of ["ahead", "identical", "behind", "diverged", "unknown"]) {
      const fixture = candidateArchive(t);
      const result = runReuse(script, fixture, { COMPARE_STATUS: status });
      if (status === "ahead" || status === "identical") {
        assert.equal(result.status, 0, result.stderr);
        assert.equal(result.output, `digest=${fixture.sha}\n`);
      } else {
        assert.notEqual(result.status, 0, status);
        assert.equal(result.output, "");
      }
    }
  });

  it("rejects a valid candidate supplied under another run identifier", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
    const result = runReuse(script, candidateArchive(t), { CANDIDATE_RUN_ID: "123" });
    assert.notEqual(result.status, 0);
    assert.equal(result.output, "");
  });

  it("rejects unsuccessful runs and foreign workflows despite valid artifact bytes", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
    for (const changes of [
      { status: "in_progress" },
      { conclusion: "failure" },
      { path: ".github/workflows/untrusted.yml" },
    ]) {
      const fixture = candidateArchive(t);
      const runFile = path.join(fixture.dir, "candidate-source-run.json");
      writeFileSync(runFile, JSON.stringify({ ...JSON.parse(readFileSync(runFile, "utf8")), ...changes }));
      const result = runReuse(script, fixture);
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
    }
  });

  it("rejects an altered archive before publishing its digest", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
    const result = runReuse(script, candidateArchive(t, { archiveCorrupt: true }));
    assert.notEqual(result.status, 0);
    assert.equal(result.output, "");
  });

  for (const sums of ["desktop-SHA256SUMS", "desktop-staging-SHA256SUMS", "scaffold-SHA256SUMS", "SHA256SUMS"]) {
    it(`rejects bad ${sums} even with a valid archive digest`, async (t) => {
      const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
      const result = runReuse(script, candidateArchive(t, { corrupt: sums }));
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
    });
  }

  it("rejects old candidates without a staging manifest", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
    const result = runReuse(script, candidateArchive(t, { missingStaging: true }));
    assert.notEqual(result.status, 0);
    assert.equal(result.output, "");
  });
});

function publicationFixture(t) {
  const fixture = candidateArchive(t);
  const { dir } = fixture;
  mkdirSync(path.join(dir, "candidate"));
  fixtureCommand(dir, "tar", "-xzf", path.join(dir, "reused/release-candidate.tar.gz"), "-C", path.join(dir, "candidate"));
  copyFileSync(path.join(root, "scripts/guard-scaffold-runtime-release.mjs"), path.join(dir, "scripts/guard-scaffold-runtime-release.mjs"));
  const bucket = path.join(dir, "bucket");
  mkdirSync(bucket);
  // Only the storage boundary is simulated; execute the actual publisher scripts.
  writeFileSync(path.join(dir, "bin/gcloud"), `#!${process.execPath}
const fs = require("node:fs");
const path = require("node:path");
const [service, action, ...args] = process.argv.slice(2);
const local = value => value.startsWith("gs://") ? path.join(process.env.FIXTURE_BUCKET, value.slice(5).split("/").slice(1).join("/")) : value;
try {
  if (service !== "storage") throw new Error("unexpected service");
  if (action === "cat") process.stdout.write(fs.readFileSync(local(args[0])));
  else if (action === "cp") {
    const immutable = args[0] === "--if-generation-match=0";
    if (args.at(-1).startsWith("gs://")) fs.appendFileSync(process.env.FIXTURE_WRITES, JSON.stringify(args) + "\\n");
    const [source, target] = args.slice(immutable ? 1 : 0).map(local);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.copyFileSync(source, target, immutable ? fs.constants.COPYFILE_EXCL : 0);
  } else throw new Error("unexpected action");
} catch (error) { console.error(error.message); process.exit(1); }
`, { mode: 0o755 });
  const env = {
    PATH: `${path.join(dir, "bin")}${path.delimiter}${process.env.PATH}`,
    FIXTURE_BUCKET: bucket, FIXTURE_WRITES: path.join(dir, "storage-writes.jsonl"),
    COMET_RELEASES_GCS_BUCKET: "ashler-comet-releases-staging", RUNNER_TEMP: dir,
    VERSION, RELEASE_SURFACE: "desktop-and-scaffold", PROMOTION_TARGET: "staging",
    SCAFFOLD_RUNTIME_DEPLOYMENT: "production-deployed",
  };
  return { dir, bucket, env };
}

describe("release channel publication", () => {
  it("rejects non-staging buckets before any storage write for candidates or channel promotion", async (t) => {
    const script = shellStep(await read(".github/workflows/release.yml"), "publish-staging", "publish");
    for (const promotion of ["staging-candidate", "staging"]) {
      for (const configuredBucket of [undefined, "", "ashler-comet-releases-production", "unrelated-private-bucket", "ashler-comet-releases-staging-copy"]) {
        const { dir, bucket, env } = publicationFixture(t);
        const result = command(dir, "bash", ["-c", script], {
          ...env, PROMOTION_TARGET: promotion, COMET_RELEASES_GCS_BUCKET: configuredBucket,
        });
        assert.notEqual(result.status, 0);
        assert.equal(existsSync(env.FIXTURE_WRITES), false);
        assert.deepEqual(readdirSync(bucket), []);
      }
    }
  });

  it("publishes both staging channels but keeps staging files out of production", async (t) => {
    const workflow = await read(".github/workflows/release.yml");
    for (const target of ["staging", "production"]) {
      const { dir, bucket, env } = publicationFixture(t);
      const result = command(dir, "bash", ["-c", shellStep(workflow, `publish-${target}`, "publish")], env);
      assert.equal(result.status, 0, result.stderr);
      const releases = path.join(bucket, "releases");
      const expected = readdirSync(path.join(dir, "candidate")).filter(name =>
        target === "staging" || !/^(comet-staging-|desktop-staging-)/.test(name));
      assert.deepEqual(readdirSync(path.join(releases, VERSION)).sort(), expected.sort());
      for (const name of expected) {
        assert.deepEqual(readFileSync(path.join(releases, VERSION, name)), readFileSync(path.join(dir, "candidate", name)));
        if (name === "COMET_SHA256SUMS" || name === "provenance.json") {
          assert.equal(existsSync(path.join(releases, name)), false);
        } else {
          assert.deepEqual(readFileSync(path.join(releases, name)), readFileSync(path.join(dir, "candidate", name)));
        }
      }
      assert.equal(readFileSync(path.join(releases, "desktop-latest.txt"), "utf8"), VERSION);
      assert.equal(existsSync(path.join(releases, "desktop-staging-latest.txt")), target === "staging");
      if (target === "production") {
        assert.deepEqual(readdirSync(releases).filter(name => /^(comet-staging-|desktop-staging-)/.test(name)), []);
      } else {
        const readback = shellStep(workflow, "publish-staging", "readback");
        assert.equal(command(dir, "bash", ["-c", readback], env).status, 0);
        writeFileSync(path.join(releases, "desktop-staging-latest.txt"), "0.0.1");
        assert.notEqual(command(dir, "bash", ["-c", readback], env).status, 0);
        writeFileSync(path.join(releases, "desktop-staging-latest.txt"), VERSION);
        writeFileSync(path.join(releases, "desktop-staging-manifest.json"), "{}");
        assert.notEqual(command(dir, "bash", ["-c", readback], env).status, 0);
      }
    }
  });

  it("keeps private candidates immutable without moving either desktop channel", async (t) => {
    const { dir, bucket, env } = publicationFixture(t);
    const script = shellStep(await read(".github/workflows/release.yml"), "publish-staging", "publish");
    const result = command(dir, "bash", ["-c", script], { ...env, PROMOTION_TARGET: "staging-candidate" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(readdirSync(path.join(bucket, "releases")), [VERSION]);
    assert.deepEqual(readdirSync(path.join(bucket, "releases", VERSION)).sort(), readdirSync(path.join(dir, "candidate")).sort());
  });

  it("does not move either desktop channel when staging would move backwards", async (t) => {
    const { dir, bucket, env } = publicationFixture(t);
    const releases = path.join(bucket, "releases");
    mkdirSync(releases);
    writeFileSync(path.join(releases, "desktop-latest.txt"), VERSION);
    writeFileSync(path.join(releases, "desktop-staging-latest.txt"), "9.0.0");
    // dpkg reports that the proposed version is not newer than the existing one.
    writeFileSync(path.join(dir, "bin/dpkg"), "#!/bin/sh\nexit 1\n", { mode: 0o755 });
    const script = shellStep(await read(".github/workflows/release.yml"), "publish-staging", "publish");
    const result = command(dir, "bash", ["-c", script], env);
    assert.notEqual(result.status, 0);
    assert.equal(readFileSync(path.join(releases, "desktop-staging-latest.txt"), "utf8"), "9.0.0");
    assert.equal(existsSync(path.join(releases, "desktop-manifest.json")), false);
    assert.equal(existsSync(path.join(releases, "desktop-staging-manifest.json")), false);
  });

  it("does not move desktop channels when the selected Scaffold channel would move backwards", async (t) => {
    const { dir, bucket, env } = publicationFixture(t);
    const releases = path.join(bucket, "releases");
    mkdirSync(releases);
    writeFileSync(path.join(releases, "scaffold-latest.txt"), "9.0.0");
    writeFileSync(path.join(dir, "bin/dpkg"), "#!/bin/sh\nexit 1\n", { mode: 0o755 });
    const script = shellStep(await read(".github/workflows/release.yml"), "publish-staging", "publish");
    const result = command(dir, "bash", ["-c", script], env);
    assert.notEqual(result.status, 0);
    assert.equal(readFileSync(path.join(releases, "scaffold-latest.txt"), "utf8"), "9.0.0");
    for (const channel of ["desktop", "desktop-staging"]) {
      assert.equal(existsSync(path.join(releases, `${channel}-manifest.json`)), false);
      assert.equal(existsSync(path.join(releases, `${channel}-latest.txt`)), false);
    }
  });
});

describe("Crew edge deployment", () => {
  it("accepts only the dispatched reviewed source before installing candidate tooling", async (t) => {
    const script = shellStep(await read(".github/workflows/deploy.yml"), "candidate", "source_request");
    const dir = scratch(t);
    for (const source of [RUN_SHA, "c".repeat(40), "", "HEAD", RUN_SHA.toUpperCase()]) {
      for (const target of ["candidate", "staging", "production", ""]) {
        const result = command(dir, "bash", ["-c", script], {
          SOURCE_SHA: source, GITHUB_SHA: RUN_SHA, TARGET: target,
        });
        assert.equal(result.status === 0,
          source === RUN_SHA && ["candidate", "staging"].includes(target), result.stderr);
      }
    }
  });

  const complete = {
    CLOUDFLARE_API_TOKEN: "cloudflare-token",
    CLOUDFLARE_ACCOUNT_ID: "cloudflare-account",
    COMET_RELEASES_GCS_BUCKET: "private-releases",
    GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "reader@example.iam.gserviceaccount.com",
    GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY:
      "-----BEGIN PRIVATE KEY-----\nprivate\n-----END PRIVATE KEY-----",
  };


  it("rejects missing, partial or malformed configuration before upload", async () => {
    for (const env of [
      {},
      { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "", GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "" },
      { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "" },
      { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "" },
      { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "invalid" },
      { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "not-a-key" },
      { ...complete, CLOUDFLARE_API_TOKEN: "" },
      { ...complete, CLOUDFLARE_ACCOUNT_ID: "" },
      { ...complete, COMET_RELEASES_GCS_BUCKET: "" },
    ]) {
      assert.throws(() => releaseFeedSecrets("staging", env));
      let uploaded = false;
      await assert.rejects(syncReleaseFeedSecrets("staging", env, {
        upload: async () => { uploaded = true; },
      }));
      assert.equal(uploaded, false);
    }
  });

  it("rejects production and unspecified targets before upload", async () => {
    for (const target of ["production", undefined, ""]) {
      assert.throws(() => releaseFeedSecrets(target, complete));
      let uploaded = false;
      await assert.rejects(syncReleaseFeedSecrets(target, complete, {
        upload: async () => { uploaded = true; },
      }));
      assert.equal(uploaded, false);
    }
  });

  it("uploads one complete atomic secret payload and cleans up on success or failure", async () => {
    for (const fail of [false, true]) {
      let uploaded;
      let temporaryFile;
      let count = 0;
      const uploadError = new Error("upload unavailable");
      const result = syncReleaseFeedSecrets("staging", complete, {
        upload: async (_edgeDir, target, secretsFile) => {
          count += 1;
          temporaryFile = secretsFile;
          assert.equal((await stat(secretsFile)).mode & 0o777, 0o600);
          uploaded = { target, payload: JSON.parse(await readFile(secretsFile, "utf8")) };
          if (fail) throw uploadError;
        },
      });
      if (fail) await assert.rejects(result, error => error === uploadError);
      else assert.equal(await result, true);
      assert.equal(count, 1);
      assert.deepEqual(uploaded, {
        target: "staging",
        payload: {
          COMET_RELEASES_GCS_BUCKET: complete.COMET_RELEASES_GCS_BUCKET,
          GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: complete.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL,
          GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: complete.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY,
        },
      });
      assert.equal(existsSync(temporaryFile), false);
      assert.equal(existsSync(path.dirname(temporaryFile)), false);
    }
  });
});



describe("release candidate reuse validation", () => {
  it("accepts an exact successful candidate from main", () => {
    assert.deepEqual(validateReleaseCandidateReuse(reusableCandidate()), {
      commit: RUN_SHA,
      workflowRun: RUN_URL,
    });
  });

  it("requires staging artifacts from the same source, version, and runtime", () => {
    const missing = reusableCandidate({ desktopStagingManifest: undefined });
    assert.throws(() => validateReleaseCandidateReuse(missing));
    for (const mutate of [
      (staging, candidate) => { staging.files = candidate.desktopManifest.files; },
      (staging) => { staging.source.commit = "c".repeat(40); },
      (staging) => { staging.version = "9.0.0"; },
      (staging) => { staging.scaffoldRuntimeVersion = "scaffold.comet-runtime.v2"; },
      (staging) => { staging.releaseSurface = "desktop"; },
    ]) {
      const candidate = reusableCandidate();
      mutate(candidate.desktopStagingManifest, candidate);
      assert.throws(() => validateReleaseCandidateReuse(candidate));
    }
  });

  it("accepts a candidate branch commit only after main contains it", () => {
    const merged = reusableCandidate({ sourceCommitReachable: true });
    merged.run.head_branch = "fix/crew-installed-stability";
    assert.deepEqual(validateReleaseCandidateReuse(merged), {
      commit: RUN_SHA,
      workflowRun: RUN_URL,
    });

    const unmerged = reusableCandidate();
    unmerged.run.head_branch = "fix/crew-installed-stability";
    assert.throws(() => validateReleaseCandidateReuse(unmerged));
  });

  it("rejects candidates from another repository, commit, or file set", () => {
    const wrongRepository = reusableCandidate();
    wrongRepository.run.repository.full_name = "attacker/comet";
    assert.throws(() => validateReleaseCandidateReuse(wrongRepository));

    const wrongCommit = reusableCandidate();
    wrongCommit.desktopManifest.source.commit = "c".repeat(40);
    assert.throws(() => validateReleaseCandidateReuse(wrongCommit));

    const wrongFiles = reusableCandidate();
    delete wrongFiles.unifiedManifest.files[`comet-${VERSION}-linux-aarch64.tar.gz`];
    assert.throws(() => validateReleaseCandidateReuse(wrongFiles));
  });
});

describe("release bucket privacy", () => {
  for (const job of ["publish-staging", "publish-production"]) {
    it(`${job} requires uniform access, public-access prevention and private IAM`, async (t) => {
      const script = shellStep(await read(".github/workflows/release.yml"), job, "privacy");
      const privateConfig = { iamConfiguration: {
        uniformBucketLevelAccess: { enabled: true }, publicAccessPrevention: "enforced",
      } };
      for (const [config, publicIam, accepted] of [
        [privateConfig, false, true],
        [{ iamConfiguration: { uniformBucketLevelAccess: { enabled: false }, publicAccessPrevention: "enforced" } }, false, false],
        [{ iamConfiguration: { uniformBucketLevelAccess: { enabled: true }, publicAccessPrevention: "inherited" } }, false, false],
        [privateConfig, true, false],
        [{}, false, false],
      ]) {
        const dir = scratch(t);
        const bin = path.join(dir, "bin");
        mkdirSync(bin);
        writeFileSync(path.join(bin, "gcloud"), `#!${process.execPath}
const args = process.argv.slice(2);
if (args[0] === "auth" && args[1] === "print-access-token") console.log("test-only-no-authority");
else if (args[0] === "storage" && args[1] === "buckets" && args[2] === "get-iam-policy") {
  console.log(JSON.stringify({ bindings: process.env.FIXTURE_PUBLIC_IAM === "1"
    ? [{ role: "roles/storage.objectViewer", members: ["allUsers"] }] : [] }));
} else process.exit(90);
`, { mode: 0o755 });
        writeFileSync(path.join(bin, "curl"), `#!${process.execPath}
require("node:fs").readFileSync(0);
const url = process.argv.at(-1);
if (url.includes("/iam/testPermissions?")) console.log(JSON.stringify({ permissions:
  ["storage.buckets.get", "storage.buckets.getIamPolicy", "storage.objects.create", "storage.objects.delete", "storage.objects.get", "storage.objects.list"] }));
else if (url.endsWith("?fields=iamConfiguration")) console.log(process.env.FIXTURE_BUCKET_CONFIG);
else process.exit(91);
`, { mode: 0o755 });
        const result = command(dir, "bash", ["-c", script], {
          PATH: `${bin}${path.delimiter}${process.env.PATH}`,
          COMET_RELEASES_GCS_BUCKET: "test-private-release",
          FIXTURE_BUCKET_CONFIG: JSON.stringify(config), FIXTURE_PUBLIC_IAM: publicIam ? "1" : "0",
        });
        assert.equal(result.status === 0, accepted, result.stderr);
      }
    });
  }
});
