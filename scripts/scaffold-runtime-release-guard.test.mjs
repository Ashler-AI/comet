import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { validateRuntimePromotion } from "./guard-scaffold-runtime-release.mjs";

const manifest = (scaffoldRuntimeVersion) => ({ scaffoldRuntimeVersion });

describe("Scaffold runtime release guard", () => {
  it("allows unchanged runtime contracts without deployment acknowledgement", () => {
    assert.deepEqual(validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v1"),
      target: "production",
      acknowledgement: "unchanged",
      releaseSurface: "desktop",
    }), {
      changed: false,
      candidateVersion: "scaffold.comet-runtime.v1",
      currentVersion: "scaffold.comet-runtime.v1",
    });
  });

  it("requires Scaffold staging before publishing a changed staging runtime", () => {
    assert.throws(() => validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v2"),
      target: "staging",
      acknowledgement: "unchanged",
      releaseSurface: "desktop-and-scaffold",
    }));

    assert.equal(validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v2"),
      target: "staging",
      acknowledgement: "staging-deployed",
      releaseSurface: "desktop-and-scaffold",
    }).changed, true);
  });

  it("requires Scaffold production before publishing a changed production runtime", () => {
    assert.throws(() => validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v2"),
      target: "production",
      acknowledgement: "staging-deployed",
      releaseSurface: "desktop-and-scaffold",
    }));

    assert.equal(validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v2"),
      target: "production",
      acknowledgement: "production-deployed",
      releaseSurface: "desktop-and-scaffold",
    }).changed, true);
  });

  it("blocks desktop-only publication when the runtime contract changes", () => {
    assert.throws(() => validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: manifest("scaffold.comet-runtime.v2"),
      target: "staging",
      acknowledgement: "staging-deployed",
      releaseSurface: "desktop",
    }));
  });

  it("releases only the desktop app ahead of Scaffold when explicitly acknowledged", () => {
    for (const target of ["staging", "production"]) {
      assert.deepEqual(validateRuntimePromotion({
        candidate: manifest("scaffold.comet-runtime.v2"),
        current: manifest("scaffold.comet-runtime.v1"),
        target,
        acknowledgement: "desktop-ahead-of-scaffold",
        releaseSurface: "desktop",
      }), {
        changed: true,
        candidateVersion: "scaffold.comet-runtime.v2",
        currentVersion: "scaffold.comet-runtime.v1",
        desktopAheadOfScaffold: true,
      });
      // The acknowledgement never publishes the Scaffold runtime itself.
      assert.throws(() => validateRuntimePromotion({
        candidate: manifest("scaffold.comet-runtime.v2"),
        current: manifest("scaffold.comet-runtime.v1"),
        target,
        acknowledgement: "desktop-ahead-of-scaffold",
        releaseSurface: "desktop-and-scaffold",
      }), /cannot publish the scaffold\.comet-runtime\.v2 Scaffold runtime/);
    }
    // Without it, a desktop-only contract change still fails and names the explicit path.
    assert.throws(() => validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v2"),
      current: manifest("scaffold.comet-runtime.v1"),
      target: "production",
      acknowledgement: "production-deployed",
      releaseSurface: "desktop",
    }), /scaffold_runtime_deployment=desktop-ahead-of-scaffold/);
  });

  it("treats a missing published manifest as a guarded first runtime release", () => {
    assert.throws(() => validateRuntimePromotion({
      candidate: manifest("scaffold.comet-runtime.v1"),
      current: undefined,
      target: "staging",
      acknowledgement: "unchanged",
      releaseSurface: "desktop-and-scaffold",
    }));
  });
});
