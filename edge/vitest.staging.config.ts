import { defineConfig } from "vitest/config";

// Node drives real workerd workers. Neither legacy WASM nor native loro is
// evaluated by Vitest's restricted module runner.
export default defineConfig({
  test: {
    include: ["test/staging/**/*.test.ts"],
    environment: "node",
    fileParallelism: false,
    testTimeout: 20_000,
    hookTimeout: 30_000
  }
});
