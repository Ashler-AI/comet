import assert from "node:assert/strict";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { test } from "node:test";
import { syncReleaseFeedSecrets } from "./sync-edge-release-feed-secrets.mjs";

const complete = {
  CLOUDFLARE_API_TOKEN: "local-test-token",
  CLOUDFLARE_ACCOUNT_ID: "dd15bb23eb5c14365fe2621acc049e01",
  COMET_RELEASES_GCS_BUCKET: "private-releases",
  GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "reader@example.iam.gserviceaccount.com",
  GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "-----BEGIN PRIVATE KEY-----\nlocal-test-key\n-----END PRIVATE KEY-----",
};

test("rejects non-staging targets and missing or invalid credentials before upload", async () => {
  let uploads = 0;
  const upload = async () => { uploads++; };
  for (const target of ["production", "", undefined]) {
    await assert.rejects(syncReleaseFeedSecrets(target, complete, { upload }), /unsupported Crew edge environment/);
  }
  await assert.rejects(syncReleaseFeedSecrets("staging", {}, { upload }), /reader email and private key together/);
  for (const name of Object.keys(complete)) {
    await assert.rejects(syncReleaseFeedSecrets("staging", { ...complete, [name]: "" }, { upload }), /requires/);
  }
  for (const [name, value, message] of [
    ["CLOUDFLARE_ACCOUNT_ID", "other-account", /existing staging Cloudflare account/],
    ["GCP_RELEASE_SERVICE_ACCOUNT_EMAIL", "not-a-reader", /email is malformed/],
    ["GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY", "not-a-key", /private key is malformed/],
  ]) {
    await assert.rejects(syncReleaseFeedSecrets("staging", { ...complete, [name]: value }, { upload }), message);
  }
  assert.equal(uploads, 0);
});

test("uploads only the reader payload with the explicit migration config and removes the private file", async () => {
  let secretsFile;
  let uploads = 0;
  const edgeDir = "/local-test/edge";
  assert.equal(await syncReleaseFeedSecrets("staging", complete, {
    edgeDir,
    upload: async (cwd, args, env) => {
      uploads++;
      secretsFile = args[2];
      assert.equal(cwd, edgeDir);
      assert.equal(env, complete);
      assert.deepEqual(args, ["secret", "bulk", secretsFile, "--config", "wrangler.migration-staging.jsonc"]);
      assert.equal((await stat(secretsFile)).mode & 0o777, 0o600);
      assert.deepEqual(JSON.parse(await readFile(secretsFile, "utf8")), {
        COMET_RELEASES_GCS_BUCKET: complete.COMET_RELEASES_GCS_BUCKET,
        GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: complete.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL,
        GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: complete.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY,
      });
    },
  }), true);
  assert.equal(uploads, 1);
  await assert.rejects(stat(path.dirname(secretsFile)), { code: "ENOENT" });
});

test("propagates upload failures and still removes the private file", async () => {
  let secretsFile;
  const failure = new Error("upload failed");
  await assert.rejects(syncReleaseFeedSecrets("staging", complete, {
    upload: async (_cwd, args) => {
      secretsFile = args[2];
      throw failure;
    },
  }), error => error === failure);
  await assert.rejects(stat(path.dirname(secretsFile)), { code: "ENOENT" });
});
