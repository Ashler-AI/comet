import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const required = (env, name, target) => {
  const value = env[name]?.trim();
  if (!value) throw new Error(`${target} release-feed synchronization requires ${name}`);
  return value;
};

export function releaseFeedSecrets(target, env) {
  if (target !== "staging") throw new Error(`unsupported Crew edge environment: ${target}`);

  const email = env.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL?.trim() ?? "";
  const privateKey = env.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY?.trim() ?? "";
  if (!email || !privateKey) {
    throw new Error(`${target} release-feed synchronization requires the reader email and private key together`);
  }
  if (!email.endsWith(".iam.gserviceaccount.com")) {
    throw new Error(`${target} release-reader email is malformed`);
  }
  if (!/^-----BEGIN PRIVATE KEY-----[\s\S]+-----END PRIVATE KEY-----$/.test(privateKey)) {
    throw new Error(`${target} release-reader private key is malformed`);
  }

  required(env, "CLOUDFLARE_API_TOKEN", target);
  if (required(env, "CLOUDFLARE_ACCOUNT_ID", target) !== "dd15bb23eb5c14365fe2621acc049e01") {
    throw new Error("staging release-feed synchronization requires the existing staging Cloudflare account");
  }
  const bucket = required(env, "COMET_RELEASES_GCS_BUCKET", target);
  return {
    COMET_RELEASES_GCS_BUCKET: bucket,
    GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: email,
    GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: privateKey,
  };
}

const runWrangler = (edgeDir, args, env) =>
  new Promise((resolve, reject) => {
    const wrangler = path.join(edgeDir, "node_modules", ".bin", "wrangler");
    const child = spawn(wrangler, args, { cwd: edgeDir, env, stdio: "inherit" });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(`wrangler secret bulk failed (${signal ?? `exit ${code}`})`));
    });
  });

export async function syncReleaseFeedSecrets(
  target,
  env = process.env,
  { edgeDir = path.join(root, "edge"), upload = runWrangler } = {},
) {
  const secrets = releaseFeedSecrets(target, env);

  const directory = await mkdtemp(path.join(os.tmpdir(), "crew-release-feed-secrets-"));
  const secretsFile = path.join(directory, "secrets.json");
  try {
    await writeFile(secretsFile, `${JSON.stringify(secrets)}\n`, { mode: 0o600 });
    await upload(edgeDir, ["secret", "bulk", secretsFile, "--config", "wrangler.migration-staging.jsonc"], env);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
  return true;
}

const invokedPath = process.argv[1] ? pathToFileURL(path.resolve(process.argv[1])).href : "";
if (import.meta.url === invokedPath) {
  const target = process.argv[2];
  syncReleaseFeedSecrets(target).catch((error) => {
    console.error(`::error::${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
