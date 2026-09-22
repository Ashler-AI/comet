import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";

// Import the sandbox roster, never maintain a second list of model IDs here.
const revision = process.argv[2];
if (!/^[a-f0-9]{40}$/.test(revision ?? "")) {
  throw new Error("Usage: node scripts/sync-omp-model-catalog.mjs <platform commit SHA>");
}
const path = "internal/scaffold/control-plane/worker/src/omp-auth-broker.ts";
const source = execFileSync("gh", ["api", `repos/Ashler-AI/ashler-platform/contents/${path}?ref=${revision}`, "-H", "Accept: application/vnd.github.raw+json"], { encoding: "utf8", maxBuffer: 1024 * 1024 });
const literal = source.match(/export const ompInferenceModelCatalog = (\{[\s\S]*?\}) as const;/)?.[1];
if (!literal) throw new Error("Sandbox model catalog declaration not found");
// The canonical declaration is a plain object of string arrays. Reject code.
const roster = JSON.parse(literal.replace(/\b(openai|anthropic):/g, '"$1":').replace(/,\s*([\]}])/g, "$1"));
if (Object.keys(roster).sort().join() !== "anthropic,openai") throw new Error("Unknown sandbox provider");
const models = Object.entries(roster).flatMap(([provider, ids]) => {
  if (!Array.isArray(ids) || ids.length === 0 || ids.length > 100) throw new Error("Invalid sandbox roster");
  return ids.map(id => {
    if (typeof id !== "string" || !/^[a-z0-9.-]+$/.test(id)) throw new Error("Invalid model ID");
    return { id: `${provider === "openai" ? "openai-codex" : provider}/${id}`, label: id,
      description: "Scaffold model · account access checked when starting a run",
      reasoningLevels: id === "claude-opus-5-5" ? ["low", "medium", "high", "xhigh", "max"]
        : id.startsWith("claude-3-") ? [] : ["low", "medium", "high"] };
  });
});
if (new Set(models.map(model => model.id)).size !== models.length) throw new Error("Duplicate model ID");
writeFileSync(new URL("../crates/harness/src/omp/scaffold-models.json", import.meta.url), JSON.stringify({ source: { repository: "Ashler-AI/ashler-platform", revision, path }, models }, null, 2) + "\n");
console.log(`Imported ${models.length} Scaffold models from ${revision}`);
