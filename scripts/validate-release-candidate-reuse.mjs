import { readFile } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

function requireObject(value, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${label} must be an object`);
  }
  return value;
}

function requireEqual(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(`${label} is ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
  }
}

function requireExactFiles(manifest, expected, label) {
  const files = requireObject(manifest.files, `${label}.files`);
  const actual = Object.keys(files).sort();
  const wanted = [...expected].sort();
  if (JSON.stringify(actual) !== JSON.stringify(wanted)) {
    throw new Error(`${label}.files is ${JSON.stringify(actual)}, expected ${JSON.stringify(wanted)}`);
  }
  for (const name of wanted) {
    if (!/^[0-9a-f]{64}$/.test(requireObject(files[name], `${label}.files.${name}`).sha256)) {
      throw new Error(`${label}.files.${name}.sha256 is invalid`);
    }
  }
}

function validateSource(source, { repository, runId, runSha, runUrl }, label) {
  const value = requireObject(source, `${label}.source`);
  requireEqual(value.repository, repository, `${label}.source.repository`);
  requireEqual(value.commit, runSha, `${label}.source.commit`);
  requireEqual(value.workflowRun, runUrl, `${label}.source.workflowRun`);
  if (!value.workflowRun.endsWith(`/actions/runs/${runId}`)) {
    throw new Error(`${label}.source.workflowRun does not identify run ${runId}`);
  }
}

export function validateReleaseCandidateReuse({
  run,
  desktopManifest,
  scaffoldManifest,
  unifiedManifest,
  requestedVersion,
  releaseSurface,
  repository,
  runId,
}) {
  const sourceRun = requireObject(run, "source run");
  if (!/^[1-9][0-9]*$/.test(String(runId))) throw new Error("candidate run id is invalid");
  requireEqual(String(sourceRun.id), String(runId), "source run id");
  requireEqual(sourceRun.repository?.full_name, repository, "source run repository");
  requireEqual(sourceRun.head_repository?.full_name, repository, "source run head repository");
  requireEqual(sourceRun.path, ".github/workflows/release.yml", "source run workflow");
  requireEqual(sourceRun.event, "workflow_dispatch", "source run event");
  requireEqual(sourceRun.status, "completed", "source run status");
  requireEqual(sourceRun.conclusion, "success", "source run conclusion");
  requireEqual(sourceRun.head_branch, "main", "source run branch");
  if (!/^[0-9a-f]{40}$/.test(sourceRun.head_sha ?? "")) {
    throw new Error("source run head SHA is invalid");
  }
  if (typeof sourceRun.html_url !== "string" || sourceRun.html_url.length === 0) {
    throw new Error("source run URL is invalid");
  }

  const source = {
    repository,
    runId: String(runId),
    runSha: sourceRun.head_sha,
    runUrl: sourceRun.html_url,
  };
  const desktop = requireObject(desktopManifest, "desktop manifest");
  requireEqual(desktop.schemaVersion, 2, "desktop manifest schemaVersion");
  requireEqual(desktop.releaseSurface, "desktop", "desktop manifest releaseSurface");
  requireEqual(desktop.version, requestedVersion, "desktop manifest version");
  validateSource(desktop.source, source, "desktop manifest");
  if (!/^scaffold\.comet-runtime\.v[1-9][0-9]*$/.test(desktop.scaffoldRuntimeVersion ?? "")) {
    throw new Error("desktop manifest Scaffold runtime version is invalid");
  }
  requireExactFiles(desktop, [
    `comet-${requestedVersion}-macos-arm64.dmg`,
    `comet-${requestedVersion}-macos-arm64-app.tar.gz`,
  ], "desktop manifest");

  if (releaseSurface === "desktop") {
    if (scaffoldManifest !== undefined || unifiedManifest !== undefined) {
      throw new Error("desktop-only reuse must not include Scaffold manifests");
    }
  } else if (releaseSurface === "desktop-and-scaffold") {
    const scaffold = requireObject(scaffoldManifest, "Scaffold manifest");
    requireEqual(scaffold.schemaVersion, 2, "Scaffold manifest schemaVersion");
    requireEqual(scaffold.releaseSurface, "scaffold", "Scaffold manifest releaseSurface");
    requireEqual(scaffold.version, requestedVersion, "Scaffold manifest version");
    requireEqual(
      scaffold.scaffoldRuntimeVersion,
      desktop.scaffoldRuntimeVersion,
      "Scaffold manifest runtime version",
    );
    validateSource(scaffold.source, source, "Scaffold manifest");
    requireExactFiles(scaffold, [
      `comet-${requestedVersion}-linux-aarch64.tar.gz`,
      `comet-${requestedVersion}-linux-x86_64.tar.gz`,
    ], "Scaffold manifest");

    const unified = requireObject(unifiedManifest, "unified manifest");
    requireEqual(unified.schemaVersion, 1, "unified manifest schemaVersion");
    requireEqual(unified.version, requestedVersion, "unified manifest version");
    validateSource(unified.source, source, "unified manifest");
    requireExactFiles(unified, [
      `comet-${requestedVersion}-linux-aarch64.tar.gz`,
      `comet-${requestedVersion}-linux-x86_64.tar.gz`,
      `comet-${requestedVersion}-macos-arm64.dmg`,
      `comet-${requestedVersion}-macos-arm64-app.tar.gz`,
    ], "unified manifest");
  } else {
    throw new Error(`unsupported release surface: ${releaseSurface}`);
  }

  return { commit: sourceRun.head_sha, workflowRun: sourceRun.html_url };
}

async function readJson(file) {
  return JSON.parse(await readFile(file, "utf8"));
}

async function main() {
  const [runPath, candidateDir, requestedVersion, releaseSurface, repository, runId] = process.argv.slice(2);
  if (!runPath || !candidateDir || !requestedVersion || !releaseSurface || !repository || !runId) {
    throw new Error(
      "usage: validate-release-candidate-reuse.mjs <run-json> <candidate-dir> <version> <release-surface> <repository> <run-id>",
    );
  }
  const hasScaffold = releaseSurface === "desktop-and-scaffold";
  const result = validateReleaseCandidateReuse({
    run: await readJson(runPath),
    desktopManifest: await readJson(path.join(candidateDir, "desktop-manifest.json")),
    scaffoldManifest: hasScaffold
      ? await readJson(path.join(candidateDir, "scaffold-manifest.json"))
      : undefined,
    unifiedManifest: hasScaffold
      ? await readJson(path.join(candidateDir, "manifest.json"))
      : undefined,
    requestedVersion,
    releaseSurface,
    repository,
    runId,
  });
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
