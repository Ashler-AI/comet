import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (relative) => readFile(path.join(root, relative), "utf8");

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
      /if: \$\{\{ needs\.version\.outputs\.release_surface == 'desktop-and-scaffold' \}\}/,
    );
    assert.doesNotMatch(jobBlock(workflow, "macos"), /release_surface == 'desktop-and-scaffold'/);
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

  it("blocks runtime version bumps until Scaffold is deployed first", async () => {
    const workflow = await read(".github/workflows/release.yml");
    assert.match(workflow, /scaffold_runtime_deployment:[\s\S]*default: unchanged[\s\S]*- staging-deployed[\s\S]*- production-deployed/);
    for (const [job, target] of [["publish-staging", "staging"], ["publish-production", "production"]]) {
      const block = jobBlock(workflow, job);
      const guard = block.indexOf("node scripts/guard-scaffold-runtime-release.mjs");
      const publish = block.indexOf("for file in candidate/*");
      assert.notEqual(guard, -1, `${job} is missing the runtime release guard`);
      assert.ok(guard < publish, `${job} must guard before publishing immutable objects`);
      assert.match(
        block,
        new RegExp(
          `candidate/desktop-manifest\\.json[\\s\\S]*${target}[\\s\\S]*\\$SCAFFOLD_RUNTIME_DEPLOYMENT[\\s\\S]*\\$RELEASE_SURFACE`,
        ),
      );
    }
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
