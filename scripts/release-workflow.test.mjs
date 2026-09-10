import assert from "node:assert/strict";
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
