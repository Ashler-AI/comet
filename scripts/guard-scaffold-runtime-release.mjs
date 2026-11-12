import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

// desktop-ahead-of-scaffold releases only the desktop channel while Scaffold keeps
// its deployed runtime. Desktop reaches Scaffold sandboxes only through their
// supervisor, and refuses a mismatched one with scaffold_runtime_contract_mismatch.
const DESKTOP_AHEAD = "desktop-ahead-of-scaffold";
const acknowledgements = new Set(["unchanged", "staging-deployed", "production-deployed", DESKTOP_AHEAD]);
const targets = new Set(["staging", "production"]);
const releaseSurfaces = new Set(["desktop", "desktop-and-scaffold"]);

function runtimeVersion(manifest, label) {
  const version = manifest?.scaffoldRuntimeVersion;
  if (typeof version !== "string" || !/^scaffold\.comet-runtime\.v[1-9][0-9]*$/.test(version)) {
    throw new Error(`${label} has no valid scaffoldRuntimeVersion`);
  }
  return version;
}

export function validateRuntimePromotion({
  candidate,
  current,
  target,
  acknowledgement,
  releaseSurface,
}) {
  if (!targets.has(target)) throw new Error(`unsupported promotion target: ${target}`);
  if (!acknowledgements.has(acknowledgement)) {
    throw new Error(`unsupported Scaffold deployment acknowledgement: ${acknowledgement}`);
  }
  if (!releaseSurfaces.has(releaseSurface)) {
    throw new Error(`unsupported release surface: ${releaseSurface}`);
  }

  const candidateVersion = runtimeVersion(candidate, "candidate Comet manifest");
  const currentVersion = current === undefined
    ? undefined
    : runtimeVersion(current, "current Scaffold manifest");
  if (currentVersion === candidateVersion) return { changed: false, candidateVersion, currentVersion };

  if (acknowledgement === DESKTOP_AHEAD) {
    if (releaseSurface !== "desktop") {
      throw new Error([
        `${DESKTOP_AHEAD} publishes only the desktop channel; it cannot publish the ${candidateVersion} Scaffold runtime.`,
        "Use release_surface=desktop, or deploy compatible ashler-platform support and acknowledge that deployment.",
      ].join(" "));
    }
    return { changed: true, candidateVersion, currentVersion, desktopAheadOfScaffold: true };
  }

  if (releaseSurface !== "desktop-and-scaffold") {
    throw new Error([
      `Scaffold runtime contract changes from ${currentVersion ?? "<unpublished>"} to ${candidateVersion}.`,
      "A breaking contract release must publish desktop and Scaffold runtimes together.",
      "Rerun release.yml with release_surface=desktop-and-scaffold after deploying compatible ashler-platform support,",
      `or release only the desktop app ahead of Scaffold with scaffold_runtime_deployment=${DESKTOP_AHEAD}.`,
    ].join(" "));
  }

  const accepted = target === "staging"
    ? acknowledgement === "staging-deployed" || acknowledgement === "production-deployed"
    : acknowledgement === "production-deployed";
  if (!accepted) {
    const required = target === "staging" ? "staging-deployed" : "production-deployed";
    throw new Error([
      `Scaffold runtime contract changes from ${currentVersion ?? "<unpublished>"} to ${candidateVersion}.`,
      `Deploy compatible ashler-platform Scaffold support to ${target} before publishing Comet.`,
      `Then rerun release.yml with release_surface=desktop-and-scaffold and scaffold_runtime_deployment=${required}.`,
      "Required order: deploy and verify Scaffold first, then publish the Comet runtime that requires it.",
    ].join(" "));
  }
  return { changed: true, candidateVersion, currentVersion };
}

async function readJson(path, required) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (!required && error?.code === "ENOENT") return undefined;
    throw error;
  }
}

async function main() {
  const [candidatePath, currentPath, target, acknowledgement, releaseSurface] = process.argv.slice(2);
  if (!candidatePath || !currentPath || !target || !acknowledgement || !releaseSurface) {
    throw new Error(
      "usage: guard-scaffold-runtime-release.mjs <candidate> <current> <target> <acknowledgement> <release-surface>",
    );
  }
  const result = validateRuntimePromotion({
    candidate: await readJson(candidatePath, true),
    current: await readJson(currentPath, false),
    target,
    acknowledgement,
    releaseSurface,
  });
  if (result.desktopAheadOfScaffold) {
    console.log(`Accepted ${target} desktop ${result.candidateVersion} ahead of Scaffold ${result.currentVersion ?? "<unpublished>"}; the Scaffold channel stays unchanged.`);
  } else if (result.changed) {
    console.log(`Accepted ${target} Scaffold runtime change to ${result.candidateVersion}.`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
