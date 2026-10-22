import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

export function validateRetainedReleaseReaderBindings(bindings) {
  if (!Array.isArray(bindings)) throw new Error("invalid staging secret-list metadata");
  for (const name of [
    "COMET_RELEASES_GCS_BUCKET",
    "GCP_RELEASE_SERVICE_ACCOUNT_EMAIL",
    "GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY",
  ]) {
    const matches = bindings.filter((binding) => binding?.name === name);
    if (matches.length !== 1 || matches[0].type !== "secret_text") {
      throw new Error(`canonical staging requires one retained secret_text binding: ${name}`);
    }
  }
}

const invokedPath = process.argv[1] ? pathToFileURL(path.resolve(process.argv[1])).href : "";
if (import.meta.url === invokedPath) {
  try {
    if (process.env.CLOUDFLARE_ACCOUNT_ID !== "dd15bb23eb5c14365fe2621acc049e01") {
      throw new Error("release-reader preflight requires the canonical staging Cloudflare account");
    }
    const edgeDir = path.join(root, "edge");
    const metadata = execFileSync(path.join(edgeDir, "node_modules", ".bin", "wrangler"), [
      "secret", "list",
      "--config", "wrangler.jsonc", "--env", "staging", "--format", "json",
    ], {
      cwd: edgeDir,
      env: { ...process.env, CI: "true" },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
    });
    validateRetainedReleaseReaderBindings(JSON.parse(metadata));
    console.log("Canonical staging release-reader secret bindings are retained.");
  } catch (error) {
    console.error(`::error::${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}
