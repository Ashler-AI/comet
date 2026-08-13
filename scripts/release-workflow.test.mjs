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
    const workflow = await read(".github/workflows/release.yml");
    const candidate = jobBlock(workflow, "candidate");
    const staging = jobBlock(workflow, "publish-staging");

    assert.match(candidate, /desktop-manifest\.json/);
    assert.match(candidate, /releaseSurface:"desktop"/);
    assert.match(candidate, /if \[\[ "\$RELEASE_SURFACE" == "desktop-and-scaffold" \]\]; then[\s\S]*scaffold-manifest\.json/);
    assert.match(candidate, /scaffoldRuntimeVersion:"scaffold\.comet-runtime\.v1"/);
    assert.match(staging, /gcloud storage cp candidate\/desktop-manifest\.json .*desktop-manifest\.json/);
    assert.match(
      staging,
      /if \[\[ "\$RELEASE_SURFACE" == "desktop-and-scaffold" \]\]; then[\s\S]*gcloud storage cp candidate\/scaffold-manifest\.json .*scaffold-manifest\.json/,
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
