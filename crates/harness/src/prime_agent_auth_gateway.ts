import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const TOKEN_ENV = "OMP_AUTH_GATEWAY_TOKEN";
const DEFAULT_GATEWAY_URL = "http://127.0.0.1:4000";

type GatewayModel = {
  id: string;
  api: string;
  owned_by: string;
};

type GatewayCatalog = {
  data: GatewayModel[];
};


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

function modelConfig(model: GatewayModel) {
  const id = stripProviderPrefix(model);
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

export default async function cometAuthGateway(pi: ExtensionAPI) {
  const token = process.env[TOKEN_ENV];
  if (!token) return;

  const gatewayUrl = (process.env.PRIME_AGENT_AUTH_GATEWAY_URL ?? DEFAULT_GATEWAY_URL).replace(/\/$/, "");
  const response = await fetch(`${gatewayUrl}/v1/models`, {
    headers: { authorization: `Bearer ${token}` },
  });
  if (!response.ok) {
    throw new Error(`Comet auth gateway model discovery failed with HTTP ${response.status}`);
  }

  const catalog: unknown = await response.json();
  if (!isGatewayCatalog(catalog)) {
    throw new Error("Comet auth gateway returned an invalid model catalog");
  }

  const anthropicModels = catalog.data.filter(
    (model) => model.owned_by === "anthropic" && model.api === "anthropic-messages",
  );
  const openaiModels = catalog.data.filter(
    (model) => model.owned_by === "openai-codex" && model.api === "openai-codex-responses",
  );

  if (anthropicModels.length > 0) {
    pi.registerProvider("comet-anthropic", {
      name: "Comet Anthropic",
      baseUrl: gatewayUrl,
      apiKey: TOKEN_ENV,
      api: "anthropic-messages",
      authHeader: true,
      models: anthropicModels.map(modelConfig),
    });
  }

  if (openaiModels.length > 0) {
    pi.registerProvider("comet-openai", {
      name: "Comet OpenAI",
      baseUrl: `${gatewayUrl}/v1`,
      apiKey: TOKEN_ENV,
      api: "openai-responses",
      authHeader: true,
      models: openaiModels.map(modelConfig),
    });
  }

  pi.on("before_provider_request", (event, ctx) => {
    if (
      typeof event.payload !== "object" ||
      event.payload === null ||
      Array.isArray(event.payload)
    ) {
      return;
    }
    if (ctx.model?.provider !== "comet-anthropic" && ctx.model?.provider !== "comet-openai") return;
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
