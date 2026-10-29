import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";

const source = readFileSync(new URL("./package-linux.sh", import.meta.url), "utf8");
const installer = source.split("<<'INSTALL'\n")[1].split("\nINSTALL\n")[0];

for (const defect of ["missing", "non-executable"]) {
  test(`Linux installer rejects a ${defect} background engine before modifying the installation`, () => {
    const directory = mkdtempSync(path.join(tmpdir(), "crew-linux-payload-"));
    try {
      const script = path.join(directory, "install.sh");
      const app = path.join(directory, "comet");
      writeFileSync(script, installer);
      writeFileSync(app, "#!/bin/sh\nexit 0\n");
      chmodSync(app, 0o755);
      if (defect === "non-executable") {
        writeFileSync(path.join(directory, "zeron-engine"), "not executable", { mode: 0o644 });
      }
      const home = path.join(directory, "home");
      const result = spawnSync("bash", [script], {
        env: { ...process.env, HOME: home, ASHLER_INCREMENTAL_TSC_CHECKS: "false" },
        encoding: "utf8", timeout: 10_000,
      });
      assert.ifError(result.error);
      assert.equal(result.status, 1);
      assert.match(result.stderr, /background engine is missing or non-executable/);
      assert.equal(existsSync(home), false);
    } finally {
      rmSync(directory, { recursive: true, force: true });
    }
  });
}
