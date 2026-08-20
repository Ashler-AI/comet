import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";
import { validateReleaseCandidateReuse } from "./validate-release-candidate-reuse.mjs";

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

  it("reuses only a successful main release candidate for production promotion", async () => {
    const workflow = await read(".github/workflows/release.yml");
    const candidate = jobBlock(workflow, "candidate");

    assert.match(workflow, /candidate_run_id:[\s\S]*required: false[\s\S]*default: ""/);
    assert.match(workflow, /permissions:[\s\S]*actions: read/);
    assert.match(workflow, /candidate_run_id is only supported for production promotion/);
    assert.match(candidate, /gh api "repos\/\$GITHUB_REPOSITORY\/actions\/runs\/\$CANDIDATE_RUN_ID"/);
    assert.match(candidate, /actions\/download-artifact@v4[\s\S]*run-id: \$\{\{ inputs\.candidate_run_id \}\}/);
    assert.match(candidate, /node scripts\/validate-release-candidate-reuse\.mjs/);
    assert.match(candidate, /sha256sum --check desktop-SHA256SUMS/);
    assert.match(candidate, /sha256sum --check scaffold-SHA256SUMS/);
    assert.match(candidate, /sha256sum --check SHA256SUMS/);
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
