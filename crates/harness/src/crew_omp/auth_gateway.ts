// Crew: a managed OMP run's only providers, `comet-anthropic` and
// `comet-openai`, on the engine's Agent Auth relay. Ported from Crew's
// crates/harness/src/omp/auth_gateway.ts. The driver writes this file into
// the run's scratch directory and loads it with `--extension`; the relay's
// `GET /v1/models` names the one model the run's route serves.
//
// OMP logs a hook's exception and sends the request anyway, so every refusal
// here ends the process instead, as Crew's Google route does. So does a
// failure to load, which could otherwise leave OMP on providers of its own.
import { writeSync } from "node:fs";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const TOKEN_ENV = "OMP_AUTH_GATEWAY_TOKEN";
const URL_ENV = "OMP_AUTH_GATEWAY_URL";

type GatewayModel = {
  id: string;
  api: string;
  owned_by: string;
};

type GatewayCatalog = {
  data: GatewayModel[];
};

function fail(message: string): never {
  try {
    // Synchronous, so the reason reaches the engine's stderr tail before exit.
    writeSync(2, `Crew Agent Auth: ${message}\n`);
  } finally {
    process.exit(1);
  }
}

function isGatewayCatalog(value: unknown): value is GatewayCatalog {
  if (
    typeof value !== "object" ||
    value === null ||
    !("data" in value) ||
    !Array.isArray(value.data)
  ) {
    return false;
  }
  return value.data.every(
    (model) =>
      typeof model === "object" &&
      model !== null &&
      "id" in model &&
      typeof model.id === "string" &&
      "api" in model &&
      typeof model.api === "string" &&
      "owned_by" in model &&
      typeof model.owned_by === "string",
  );
}

function stripProviderPrefix(model: GatewayModel): string {
  const prefix = `${model.owned_by}/`;
  return model.id.startsWith(prefix) ? model.id.slice(prefix.length) : model.id;
}

// Models whose OpenAI model card gives a 1,050,000-token context and 128,000
// output tokens, with base per-million-token rates. Above 272K input the
// provider charges 2x input/cache and 1.5x output.
const OPENAI_MODEL_CARD_COSTS = new Map([
  ["gpt-6-sol", { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2.5 }],
  ["gpt-6.1-sol", { input: 2, output: 10, cacheRead: 0.1, cacheWrite: 2.5 }],
  ["gpt-6.2-sol", { input: 2, output: 10, cacheRead: 0.1, cacheWrite: 2.5 }],
  ["gpt-6.2-astra", { input: 8, output: 40, cacheRead: 0.4, cacheWrite: 10 }],
  ["gpt-6-luna", { input: 0.1, output: 0.5, cacheRead: 0.01, cacheWrite: 0.125 }],
]);

function modelConfig(model: GatewayModel) {
  const id = stripProviderPrefix(model);
  if (model.owned_by === "anthropic" && id === "claude-opus-5-5") {
    return {
      id,
      name: "Opus 5.5",
      reasoning: true,
      thinking: {
        mode: "anthropic-adaptive" as const,
        efforts: ["low", "medium", "high", "xhigh", "max"] as const,
        defaultLevel: "medium" as const,
        supportsDisplay: true,
        prefixBinding: true,
      },
      input: ["text", "image"] as Array<"text" | "image">,
      cost: { input: 4, output: 20, cacheRead: 0.2, cacheWrite: 5 },
      contextWindow: 1_000_000,
      maxTokens: 128_000,
      compat: {
        requiresThinkingEnabled: true,
        signingEndpoint: true,
        replayUnsignedThinking: false,
        // OMP otherwise silently rewrites any/tool to auto. Preserve the
        // caller's choice so the upstream rejects unsupported forced use.
        supportsForcedToolChoice: true,
      },
    };
  }
  const openAiCost = model.owned_by === "openai-codex" ? OPENAI_MODEL_CARD_COSTS.get(id) : undefined;
  if (openAiCost) {
    return {
      id,
      name: id,
      reasoning: true,
      input: ["text", "image"] as Array<"text" | "image">,
      cost: openAiCost,
      contextWindow: 1_050_000,
      maxTokens: 128_000,
    };
  }
  return {
    id,
    name: id,
    reasoning:
      model.owned_by === "openai-codex" || id.includes("claude-3-7") || !id.startsWith("claude-3-"),
    input: ["text", "image"] as Array<"text" | "image">,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
    contextWindow: model.owned_by === "openai-codex" ? 400_000 : 200_000,
    maxTokens: 64_000,
  };
}

export default async function crewAuthGateway(pi: ExtensionAPI) {
  return registerGateway(pi, TOKEN_ENV);
}

export async function registerGateway(pi: ExtensionAPI, tokenEnv: string) {
  const token = process.env[tokenEnv];
  if (!token) fail("the run has no Agent Auth token");
  const configured = process.env[URL_ENV];
  if (!configured) fail("the run has no Agent Auth relay");
  const gatewayUrl = configured.replace(/\/$/, "");

  let catalog: unknown;
  try {
    const response = await fetch(`${gatewayUrl}/v1/models`, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (!response.ok) fail(`model discovery failed with HTTP ${response.status}`);
    catalog = await response.json();
  } catch (error) {
    fail(`model discovery failed: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (!isGatewayCatalog(catalog)) fail("the relay returned an invalid model catalog");

  const anthropicModels = catalog.data.filter(
    (model) => model.owned_by === "anthropic" && model.api === "anthropic-messages",
  );
  const openaiModels = catalog.data.filter(
    (model) => model.owned_by === "openai-codex" && model.api === "openai-codex-responses",
  );
  if (anthropicModels.length === 0 && openaiModels.length === 0) {
    fail("the relay offers no model this run can use");
  }

  if (anthropicModels.length > 0) {
    pi.registerProvider("comet-anthropic", {
      name: "Crew Anthropic",
      baseUrl: gatewayUrl,
      apiKey: tokenEnv,
      api: "anthropic-messages",
      authHeader: true,
      models: anthropicModels.map(modelConfig),
    });
  }

  if (openaiModels.length > 0) {
    pi.registerProvider("comet-openai", {
      name: "Crew OpenAI",
      baseUrl: `${gatewayUrl}/v1`,
      apiKey: tokenEnv,
      api: "openai-responses",
      authHeader: true,
      // Preserve explicit strict:false; Responses otherwise requires optional task fields.
      models: openaiModels.map((model) => ({
        ...modelConfig(model),
        compat: { supportsStrictMode: true },
      })),
    });
  }

  pi.on("before_provider_request", (event, ctx) => {
    if (ctx.model?.provider !== "comet-anthropic" && ctx.model?.provider !== "comet-openai") {
      // Throwing is not enough: OMP would log it and send the request anyway.
      fail("refused an inference request outside its managed route");
    }
    if (
      typeof event.payload !== "object" ||
      event.payload === null ||
      Array.isArray(event.payload)
    ) {
      return;
    }
    const payload = event.payload as Record<string, unknown>;
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;

    if (ctx.model.provider === "comet-openai") {
      if (typeof payload.prompt_cache_key === "string") return;
      return { ...payload, prompt_cache_key: sessionId };
    }

    const metadata =
      typeof payload.metadata === "object" &&
        payload.metadata !== null &&
        !Array.isArray(payload.metadata)
        ? payload.metadata as Record<string, unknown>
        : {};
    if (typeof metadata.session_id === "string") return;
    return {
      ...payload,
      metadata: { ...metadata, session_id: sessionId },
    };
  });
}
