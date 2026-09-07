import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { readFile } from "node:fs/promises";
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
  writeFileSync(path.join(upstream, "Cargo.toml"), `[workspace]\nmembers = ["."]\n[workspace.package]\nversion = "${VERSION}"\n[package]\nname = "comet"\nversion.workspace = true\nedition = "2024"\n[lib]\npath = "lib.rs"\n`);
  writeFileSync(path.join(upstream, "lib.rs"), "");
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

function candidateArchive(t, { corrupt, archiveCorrupt = false, branch = "release-fixture" } = {}) {
  const dir = scratch(t);
  const payload = path.join(dir, "payload");
  for (const name of ["payload", "reused", "scripts", "bin"]) mkdirSync(path.join(dir, name));
  copyFileSync(path.join(root, "scripts/validate-release-candidate-reuse.mjs"), path.join(dir, "scripts/validate-release-candidate-reuse.mjs"));
  const candidate = reusableCandidate();
  candidate.run.head_branch = branch;
  writeFileSync(path.join(dir, "candidate-source-run.json"), JSON.stringify(candidate.run));
  for (const name of Object.keys(candidate.unifiedManifest.files)) {
    writeFileSync(path.join(payload, name), `release artifact ${name}\n`);
  }
  for (const [manifestName, sumsName, manifest] of [
    ["desktop-manifest.json", "desktop-SHA256SUMS", candidate.desktopManifest],
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

describe("executable production release gates", () => {
  it("accepts a merged release checkout without an origin/main tracking ref", async (t) => {
    const checkout = releaseCheckout(t, true);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    const result = runVersion(script, checkout);
    assert.equal(result.status, 0, result.stderr);
    assert.ok(result.output.includes(`candidate_run_id=${RUN_ID}\n`));
    assert.notEqual(command(checkout.checkout, "git", ["rev-parse", "--verify", "refs/remotes/origin/main"]).status, 0);
  });

  it("rejects unmerged production source but still permits private candidates", async (t) => {
    const checkout = releaseCheckout(t, false);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    const production = runVersion(script, checkout);
    assert.notEqual(production.status, 0);
    assert.equal(production.output, "");
    const privateCandidate = runVersion(script, checkout, {
      REQUESTED_PROMOTION_TARGET: "none", REQUESTED_CANDIDATE_RUN_ID: "",
    });
    assert.equal(privateCandidate.status, 0, privateCandidate.stderr);
    assert.ok(privateCandidate.output.includes("promotion_target=none\n"));
  });

  it("rejects invalid reuse identifiers and nonproduction reuse dispatches", async (t) => {
    const checkout = releaseCheckout(t, true);
    const script = shellStep(await read(".github/workflows/release.yml"), "version", "version");
    for (const overrides of [
      { REQUESTED_CANDIDATE_RUN_ID: "not-a-run-id" },
      { REQUESTED_PROMOTION_TARGET: "staging" },
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

  for (const sums of ["desktop-SHA256SUMS", "scaffold-SHA256SUMS", "SHA256SUMS"]) {
    it(`rejects bad ${sums} even with a valid archive digest`, async (t) => {
      const script = shellStep(await read(".github/workflows/release.yml"), "candidate", "reuse");
      const result = runReuse(script, candidateArchive(t, { corrupt: sums }));
      assert.notEqual(result.status, 0);
      assert.equal(result.output, "");
    });
  }
});

describe("Crew edge deployment", () => {
  const complete = {
    CLOUDFLARE_API_TOKEN: "cloudflare-token",
    CLOUDFLARE_ACCOUNT_ID: "cloudflare-account",
    COMET_RELEASES_GCS_BUCKET: "private-releases",
    GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "reader@example.iam.gserviceaccount.com",
    GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY:
      "-----BEGIN PRIVATE KEY-----\nprivate\n-----END PRIVATE KEY-----",
  };

  it("routes release-feed synchronization through the matching GitHub environments before deploy", async () => {
    const workflow = await read(".github/workflows/deploy.yml");
    const staging = jobBlock(workflow, "sync-staging-release-feed-secrets");
    const production = jobBlock(workflow, "sync-production-release-feed-secrets");
    assert.match(jobBlock(workflow, "production"), /needs: \[candidate, staging, sync-production-release-feed-secrets\]/);
    assert.match(production, /needs: \[candidate, staging\]/);
    assert.match(jobBlock(workflow, "staging"), /needs: \[candidate, sync-staging-release-feed-secrets\]/);
    assert.match(staging, /needs: candidate/);
    assert.match(staging, /environment: comet-release-staging/);
    assert.match(production, /environment: comet-release-production/);
    assert.match(production, /name: Approve production edge and synchronize release-feed secrets/);
    assert.match(staging, /node scripts\/sync-edge-release-feed-secrets\.mjs staging/);
    assert.match(production, /node scripts\/sync-edge-release-feed-secrets\.mjs production/);
    assert.doesNotMatch(workflow, /wrangler secret put/);
  });

  it("preserves Worker secrets when the environment-owned reader pair is absent", async () => {
    assert.equal(
      releaseFeedSecrets("production", {
        CLOUDFLARE_API_TOKEN: "already-configured",
        CLOUDFLARE_ACCOUNT_ID: "already-configured",
        COMET_RELEASES_GCS_BUCKET: "already-configured",
      }),
      undefined,
    );
    let uploaded = false;
    const changed = await syncReleaseFeedSecrets("production", {}, {
      upload: async () => {
        uploaded = true;
      },
    });
    assert.equal(changed, false);
    assert.equal(uploaded, false);
  });

  it("rejects partial or malformed reader credentials before upload", () => {
    assert.throws(
      () => releaseFeedSecrets("production", { GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: complete.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL }),
      /reader email and private key together/,
    );
    assert.throws(
      () => releaseFeedSecrets("production", { GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: complete.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY }),
      /reader email and private key together/,
    );
    assert.throws(
      () => releaseFeedSecrets("production", { ...complete, GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "not-a-key" }),
      /private key is malformed/,
    );
    assert.throws(
      () => releaseFeedSecrets("production", { ...complete, CLOUDFLARE_API_TOKEN: "" }),
      /requires CLOUDFLARE_API_TOKEN/,
    );
  });

  it("uploads one complete atomic secret payload", async () => {
    let uploaded;
    const changed = await syncReleaseFeedSecrets("production", complete, {
      upload: async (_edgeDir, target, secretsFile) => {
        uploaded = { target, payload: JSON.parse(await readFile(secretsFile, "utf8")) };
      },
    });
    assert.equal(changed, true);
    assert.deepEqual(uploaded, {
      target: "production",
      payload: {
        COMET_RELEASES_GCS_BUCKET: complete.COMET_RELEASES_GCS_BUCKET,
        GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: complete.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL,
        GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: complete.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY,
      },
    });
  });
});

describe("Comet release surfaces", () => {
  it("builds Linux only for explicit Scaffold-capable releases while tags stay complete", async () => {
    const workflow = await read(".github/workflows/release.yml");
    assert.match(workflow, /release_surface:[\s\S]*default: desktop[\s\S]*- desktop[\s\S]*- desktop-and-scaffold/);
    assert.match(workflow, /GITHUB_REF.*refs\/tags\/v[\s\S]*release_surface="desktop-and-scaffold"/);
    assert.match(
      jobBlock(workflow, "linux"),
      /if: \$\{\{ inputs\.candidate_run_id == '' && needs\.version\.outputs\.release_surface == 'desktop-and-scaffold' \}\}/,
    );
    assert.match(jobBlock(workflow, "macos"), /if: \$\{\{ inputs\.candidate_run_id == '' \}\}/);
  });

  it("publishes a private immutable candidate without moving staging channels", async () => {
    const workflow = await read(".github/workflows/release.yml");
    const staging = jobBlock(workflow, "publish-staging");
    const production = jobBlock(workflow, "publish-production");
    assert.match(workflow, /promotion_target:[\s\S]*- staging-candidate[\s\S]*- staging[\s\S]*- production/);
    assert.match(workflow, /REQUESTED_PROMOTION_TARGET: \$\{\{ inputs\.promotion_target \}\}/);
    assert.match(workflow, /echo "promotion_target=\$promotion_target" >> "\$GITHUB_OUTPUT"/);
    assert.doesNotMatch(workflow, /github\.repository(?:_owner|_id)? ==/);
    assert.match(staging, /environment: comet-release-staging/);
    assert.match(production, /environment: comet-release-production/);
    assert.match(staging, /google-github-actions\/auth@v2/);
    assert.match(production, /google-github-actions\/auth@v2/);
    assert.match(staging, /needs\.version\.outputs\.promotion_target == 'staging-candidate'/);
    assert.match(production, /needs\.version\.outputs\.promotion_target == 'production'/);
    assert.match(staging, /always\(\)[\s\S]*needs\.version\.result == 'success'[\s\S]*needs\.candidate\.result == 'success'/);
    assert.match(production, /always\(\)[\s\S]*needs\['publish-staging'\]\.result == 'success'/);
    const immutable = staging.indexOf('publish_immutable "$file" "gs://$COMET_RELEASES_GCS_BUCKET/releases/$VERSION/$name"');
    const candidateStop = staging.indexOf('if [[ "$PROMOTION_TARGET" == "staging-candidate" ]]');
    const guard = staging.indexOf("node scripts/guard-scaffold-runtime-release.mjs");
    const channels = staging.indexOf("require_forward_version desktop");
    assert.ok(immutable >= 0 && immutable < candidateStop);
    assert.ok(candidateStop < guard && guard < channels);
  });


  it("advances desktop and Scaffold moving channels independently", async () => {
    const [workflow, runtimeVersion, engine, edge] = await Promise.all([
      read(".github/workflows/release.yml"),
      read("scaffold-runtime-version.txt"),
      read("crates/engine/src/scaffold.rs"),
      read("edge/src/auth-routes.ts"),
    ]);
    const candidate = jobBlock(workflow, "candidate");
    const staging = jobBlock(workflow, "publish-staging");
    const production = jobBlock(workflow, "publish-production");

    assert.equal(runtimeVersion, "scaffold.comet-runtime.v1");
    assert.match(engine, /SCAFFOLD_COMET_RUNTIME_VERSION[\s\S]*include_str!\("\.\.\/\.\.\/\.\.\/scaffold-runtime-version\.txt"\)/);
    assert.match(edge, /SCAFFOLD_COMET_RUNTIME_VERSION = "scaffold\.comet-runtime\.v1"/);
    assert.match(candidate, /scaffold_runtime_version="\$\(cat scaffold-runtime-version\.txt\)"/);
    assert.match(candidate, /--arg scaffoldRuntimeVersion "\$scaffold_runtime_version"/);
    assert.match(candidate, /scaffoldRuntimeVersion:\$scaffoldRuntimeVersion/);
    assert.match(candidate, /desktop-manifest\.json/);
    assert.match(candidate, /releaseSurface:"desktop"/);
    assert.match(candidate, /if \[\[ "\$RELEASE_SURFACE" == "desktop-and-scaffold" \]\]; then[\s\S]*scaffold-manifest\.json/);
    assert.match(staging, /gcloud storage cp candidate\/desktop-manifest\.json .*desktop-manifest\.json/);
    assert.match(
      staging,
      /if \[\[ "\$RELEASE_SURFACE" == "desktop-and-scaffold" \]\]; then[\s\S]*gcloud storage cp candidate\/scaffold-manifest\.json .*scaffold-manifest\.json/,
    );
    assert.match(production, /name: Verify published production channels/);
    assert.match(production, /verify_object candidate\/desktop-manifest\.json desktop-manifest\.json/);
    assert.match(
      production,
      /if \[\[ "\$RELEASE_SURFACE" == "desktop-and-scaffold" \]\]; then[\s\S]*verify_object candidate\/scaffold-manifest\.json scaffold-manifest\.json/,
    );
    assert.match(production, /verify_pointer desktop-latest\.txt[\s\S]*verify_pointer scaffold-latest\.txt[\s\S]*verify_pointer latest\.txt/);
  });

  it("blocks runtime version channel promotions until Scaffold is deployed first", async () => {
    const workflow = await read(".github/workflows/release.yml");
    assert.match(workflow, /scaffold_runtime_deployment:[\s\S]*default: unchanged[\s\S]*- staging-deployed[\s\S]*- production-deployed/);
    const staging = jobBlock(workflow, "publish-staging");
    const stagingGuard = staging.indexOf("node scripts/guard-scaffold-runtime-release.mjs");
    assert.notEqual(stagingGuard, -1);
    assert.ok(stagingGuard < staging.indexOf("require_forward_version desktop"));
    assert.match(
      staging,
      /candidate\/desktop-manifest\.json[\s\S]*staging[\s\S]*\$SCAFFOLD_RUNTIME_DEPLOYMENT[\s\S]*\$RELEASE_SURFACE/,
    );

    const production = jobBlock(workflow, "publish-production");
    const productionGuard = production.indexOf("node scripts/guard-scaffold-runtime-release.mjs");
    assert.notEqual(productionGuard, -1);
    assert.ok(productionGuard < production.indexOf("for file in candidate/*"));
    assert.match(
      production,
      /candidate\/desktop-manifest\.json[\s\S]*production[\s\S]*\$SCAFFOLD_RUNTIME_DEPLOYMENT[\s\S]*\$RELEASE_SURFACE/,
    );
  });

  it("routes desktop updates and Linux installs to compatible manifests", async () => {
    const [updater, installer, feed] = await Promise.all([
      read("crates/update/src/lib.rs"),
      read("install.sh"),
      read("edge/src/release-feed.ts"),
    ]);

    assert.match(updater, /cfg!\(target_os = "macos"\)[\s\S]*"desktop-manifest\.json"[\s\S]*"scaffold-manifest\.json"/);
    assert.match(installer, /scaffold-latest\.txt/);
    assert.match(installer, /scaffold-SHA256SUMS/);
    assert.match(feed, /\(\?:desktop\|scaffold\)/);
    assert.match(feed, /manifest\\\.json\|SHA256SUMS\|latest\\\.txt/);
  });
});

describe("release candidate reuse validation", () => {
  it("accepts an exact successful candidate from main", () => {
    assert.deepEqual(validateReleaseCandidateReuse(reusableCandidate()), {
      commit: RUN_SHA,
      workflowRun: RUN_URL,
    });
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
    assert.throws(
      () => validateReleaseCandidateReuse(unmerged),
      /commit is not reachable from main/,
    );
  });

  it("rejects candidates from another repository, commit, or file set", () => {
    const wrongRepository = reusableCandidate();
    wrongRepository.run.repository.full_name = "attacker/comet";
    assert.throws(
      () => validateReleaseCandidateReuse(wrongRepository),
      /source run repository/,
    );

    const wrongCommit = reusableCandidate();
    wrongCommit.desktopManifest.source.commit = "c".repeat(40);
    assert.throws(
      () => validateReleaseCandidateReuse(wrongCommit),
      /desktop manifest\.source\.commit/,
    );

    const wrongFiles = reusableCandidate();
    delete wrongFiles.unifiedManifest.files[`comet-${VERSION}-linux-aarch64.tar.gz`];
    assert.throws(
      () => validateReleaseCandidateReuse(wrongFiles),
      /unified manifest\.files/,
    );
  });
});
