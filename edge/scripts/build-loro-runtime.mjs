// Usage: node edge/scripts/build-loro-runtime.mjs WASM_PATH WASM_BINDGEN_PATH OUTPUT_TGZ UPSTREAM_SOURCE
// Inputs: raw patched loro_wasm.wasm, wasm-bindgen 0.2.100, and the pinned Loro
// checkout (45708d059d8620fb53066c9f86cefa1601e0e1c6). Uses integrity-pinned
// published wrappers and installed Edge tooling; never builds Rust/TypeScript.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { appendFileSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";

assert.equal(process.argv.length, 6,
  "Usage: node edge/scripts/build-loro-runtime.mjs WASM_PATH WASM_BINDGEN_PATH OUTPUT_TGZ UPSTREAM_SOURCE");
const [wasm, bindgen, output, upstream] = process.argv.slice(2).map((path) => resolve(path));
assert.equal(execFileSync(bindgen, ["--version"], { encoding: "utf8" }).trim(), "wasm-bindgen 0.2.100");
assert.equal(execFileSync("git", ["-C", upstream, "rev-parse", "HEAD"], { encoding: "utf8" }).trim(),
  "45708d059d8620fb53066c9f86cefa1601e0e1c6", "Unexpected upstream source revision");
const scripts = join(upstream, "crates/loro-wasm/scripts");
const containerPatch = readFileSync(join(scripts, "container_id_cache_patch.js"), "utf8");
const nodePatch = readFileSync(join(scripts, "nodejs_patch.js"), "utf8");
// The upstream snippet converter uses esbuild, already installed for Edge.
const esbuildModules = dirname(dirname(createRequire(import.meta.url).resolve("esbuild/package.json")));
const temporary = mkdtempSync(join(tmpdir(), "crew-loro-runtime-"));
const staging = join(temporary, "package");
try {
  const response = await fetch("https://registry.npmjs.org/loro-crdt/-/loro-crdt-1.16.4.tgz");
  assert.ok(response.ok, `Published wrapper download failed: ${response.status}`);
  const archive = Buffer.from(await response.arrayBuffer());
  assert.equal(createHash("sha512").update(archive).digest("base64"),
    "UPufMpHSEMLRVjWfCjhVbOD8nkCDwxdfVk4Uer6xTEFAcNn+X+oGYWGgIIYuAgofc8UiI0ERZUIMnP7ib/kh6w==");
  const source = join(temporary, "published");
  mkdirSync(source);
  writeFileSync(join(source, "source.tgz"), archive);
  execFileSync("tar", ["-xzf", join(source, "source.tgz"), "-C", source]);
  const published = join(source, "package");
  const original = JSON.parse(readFileSync(join(published, "package.json"), "utf8"));
  mkdirSync(staging);
  for (const target of ["nodejs", "web"]) {
    const destination = join(staging, target);
    execFileSync(bindgen, ["--weak-refs", "--target", target, "--out-name", "loro_wasm", "--out-dir", destination, wasm], {
      stdio: "inherit",
    });
    // Raw WASM imports are host-defined, not wasm-bindgen's instantiated ABI.
    // Let the consumer declare its compiled-module import (e.g. Workers).
    rmSync(join(destination, "loro_wasm_bg.wasm.d.ts"));
    // Keep published wrapper classes/helpers and their sourcemaps byte-for-byte.
    for (const file of ["index.js", "index.js.map", "index.d.ts"]) {
      copyFileSync(join(published, target, file), join(destination, file));
    }
    appendFileSync(join(destination, "loro_wasm.js"), `\n${containerPatch}`);
    if (target === "nodejs") {
      // wasm-bindgen emits ESM snippets even for Node; preserve upstream's CJS
      // conversion rather than depending on recent Node require(esm) behavior.
      execFileSync(process.execPath, [join(scripts, "nodejs-snippets.cjs"), join(destination, "snippets")], {
        stdio: "inherit",
        env: {
          ...process.env,
          NODE_PATH: [esbuildModules, process.env.NODE_PATH].filter(Boolean).join(delimiter),
        },
      });
      appendFileSync(join(destination, "loro_wasm.js"), `\n${nodePatch}`);
    } else {
      writeFileSync(join(destination, "package.json"), '{"type":"module"}\n');
      // Explicit extensions make the published declarations valid under NodeNext;
      // the published wrapper exposes default init, but its declaration omits it.
      const types = readFileSync(join(destination, "index.d.ts"), "utf8");
      writeFileSync(join(destination, "index.d.ts"),
        `${types.replaceAll('"./loro_wasm"', '"./loro_wasm.js"')}\nexport { default } from "./loro_wasm.js";\n`);
    }
  }
  copyFileSync(join(published, "LICENSE"), join(staging, "LICENSE"));
  // Deliberately do not copy upstream's browser/base64/bundler exports or build
  // scripts: only these two entry families contain the newly generated WASM.
  const manifest = {
    name: "loro-crdt",
    version: "1.16.4+crew.1",
    private: true,
    license: "MIT",
    description: original.description,
    repository: original.repository,
    type: "commonjs",
    main: "nodejs/index.js",
    types: "nodejs/index.d.ts",
    exports: {
      ".": { types: "./nodejs/index.d.ts", default: "./nodejs/index.js" },
      "./web": { types: "./web/index.d.ts", default: "./web/index.js" },
      "./nodejs": { types: "./nodejs/index.d.ts", default: "./nodejs/index.js" },
      "./web/*": "./web/*",
      "./nodejs/*": "./nodejs/*",
      "./package.json": "./package.json",
    },
    files: ["nodejs", "web", "LICENSE"],
  };
  writeFileSync(join(staging, "package.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  const [packed] = JSON.parse(execFileSync("npm", ["pack", "--ignore-scripts", "--offline", "--json", "--pack-destination", temporary], {
    cwd: staging,
    encoding: "utf8",
  }));
  assert.equal(packed.name, manifest.name);
  assert.equal(packed.version, manifest.version);
  mkdirSync(dirname(output), { recursive: true });
  copyFileSync(join(temporary, packed.filename), output);
  console.log(output);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
