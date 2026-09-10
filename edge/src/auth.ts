/**
 * Authenticates Scaffold control-plane bearers without handling Google/IAP
 * assertions in Comet. Scaffold's IAP-protected OAuth authorize route binds a
 * bearer to an internal principal; this Worker validates it on every request
 * through `/api/code-sandboxes/auth/session` so revocation is fail-closed.
 */
import type { Env } from "./env";

export type ScaffoldAuthEnv = Omit<
  Pick<
    Env,
    | "AUTH_MODE"
    | "ENVIRONMENT"
    | "SCAFFOLD_CONTROL_PLANE_URL"
    | "SCAFFOLD_PROJECT_SCOPE"
    | "SCAFFOLD_REQUIRED_CAPABILITIES"
  >,
  "SCAFFOLD_REQUIRED_CAPABILITIES"
> & {
  readonly SCAFFOLD_REQUIRED_CAPABILITIES: string;
};

export interface Verified {
  readonly userId: string;
  readonly email: string;
  readonly projectScope: string;
  readonly capabilities: readonly string[];
  readonly credential: "scaffold" | "device" | "dev";
}

export type ScaffoldAuthenticationResult =
  | { status: "authenticated"; identity: Verified }
  | { status: "invalid" }
  | { status: "unavailable" };

interface ScaffoldSession {
  readonly ok?: unknown;
  readonly resource?: unknown;
  readonly actor?: {
    readonly sub?: unknown;
    readonly auth?: unknown;
  };
  readonly scopes?: unknown;
}

const REMOTE_CODE_CAPABILITIES: Readonly<Record<string, readonly string[]>> = {
  "remote_code:create": ["session.invite"],
  "remote_code:read": ["session.read"],
  "remote_code:write": ["session.chat", "session.annotate", "session.files"],
  "remote_code:exec": ["session.control", "session.environment"],
  "remote_code:lifecycle": ["session.control", "session.environment"]
};

const capabilitiesFromRemoteCodeScopes = (scopes: readonly string[]): readonly string[] => [
  ...new Set(
    scopes.flatMap((scope) =>
      Object.hasOwn(REMOTE_CODE_CAPABILITIES, scope) ? REMOTE_CODE_CAPABILITIES[scope] : []
    )
  )
];

const LOOPBACK_HOSTS: Record<string, true> = {
  localhost: true,
  "127.0.0.1": true,
  "::1": true,
  "[::1]": true
};

/** Credential-bearing traffic is encrypted except for explicit loopback development. */
export const credentialTransportAllowed = (value: string | URL): boolean => {
  try {
    const url = typeof value === "string" ? new URL(value) : value;
    if (url.username || url.password) return false;
    if (url.protocol === "https:" || url.protocol === "wss:") return true;
    return LOOPBACK_HOSTS[url.hostname] === true && (url.protocol === "http:" || url.protocol === "ws:");
  } catch {
    return false;
  }
};

const normalizedOrigin = (value: string): string | undefined => {
  try {
    const url = new URL(value);
    if (
      !credentialTransportAllowed(url) ||
      !["http:", "https:"].includes(url.protocol) ||
      !["", "/"].includes(url.pathname) ||
      url.search ||
      url.hash
    ) {
      return undefined;
    }
    return url.origin;
  } catch {
    return undefined;
  }
};

export const requiredCapabilities = (env: ScaffoldAuthEnv): readonly string[] =>
  [...new Set(env.SCAFFOLD_REQUIRED_CAPABILITIES.split(/\s+/).map((value) => value.trim()).filter(Boolean))];

export const bearerFromRequest = (request: Request): string | undefined => {
  const header = request.headers.get("authorization");
  if (header?.toLowerCase().startsWith("bearer ")) return header.slice(7).trim() || undefined;
  const url = new URL(request.url);
  return url.searchParams.get("token")?.trim() || undefined;
};

const verifyScaffoldTokenResult = async (env: ScaffoldAuthEnv, token: string): Promise<ScaffoldAuthenticationResult> => {
  if (!token.startsWith("sc_rc_")) return { status: "invalid" };
  const resource = normalizedOrigin(env.SCAFFOLD_CONTROL_PLANE_URL);
  const projectScope = env.SCAFFOLD_PROJECT_SCOPE.trim();
  const required = requiredCapabilities(env);
  if (!resource || !projectScope || required.length === 0) return { status: "unavailable" };

  let response: Response;
  try {
    response = await fetch(`${resource}/api/code-sandboxes/auth/session`, {
      headers: { authorization: `Bearer ${token}`, accept: "application/json" }
    });
  } catch {
    return { status: "unavailable" };
  }
  // Only an explicit credential rejection is authoritative. Network errors,
  // other HTTP failures and malformed authority responses must not revoke state.
  if (response.status === 401 || response.status === 403) return { status: "invalid" };
  if (!response.ok) return { status: "unavailable" };

  let session: ScaffoldSession;
  try {
    session = (await response.json()) as ScaffoldSession;
  } catch {
    return { status: "unavailable" };
  }
  if (!session || typeof session !== "object") return { status: "unavailable" };
  const subject = typeof session.actor?.sub === "string" ? session.actor.sub.trim().toLowerCase() : "";
  const scopes = Array.isArray(session.scopes)
    ? [...new Set(session.scopes.filter((scope): scope is string => typeof scope === "string" && scope.length > 0))]
    : [];
  const capabilities = capabilitiesFromRemoteCodeScopes(scopes);
  if (
    session.ok !== true ||
    session.actor?.auth !== "iap" ||
    !subject ||
    normalizedOrigin(typeof session.resource === "string" ? session.resource : "") !== resource ||
    !required.every((capability) => capabilities.includes(capability))
  ) {
    return { status: "unavailable" };
  }
  return {
    status: "authenticated",
    identity: {
      userId: subject,
      email: subject,
      projectScope,
      capabilities,
      credential: "scaffold"
    }
  };
};

export const verifyScaffoldToken = async (env: ScaffoldAuthEnv, token: string): Promise<Verified | undefined> => {
  const result = await verifyScaffoldTokenResult(env, token);
  return result.status === "authenticated" ? result.identity : undefined;
};

export const authenticateScaffoldResult = async (env: ScaffoldAuthEnv, request: Request): Promise<ScaffoldAuthenticationResult> => {
  if (!credentialTransportAllowed(request.url)) return { status: "invalid" };
  const token = bearerFromRequest(request);
  if (!token) return { status: "invalid" };
  if (env.AUTH_MODE === "dev") {
    if (env.ENVIRONMENT !== "local") return { status: "unavailable" };
    const [userId, requestedProject] = token.split("@", 2);
    if (!userId || (requestedProject && requestedProject !== env.SCAFFOLD_PROJECT_SCOPE)) return { status: "invalid" };
    const capabilities = requiredCapabilities(env);
    if (capabilities.length === 0) return { status: "unavailable" };
    return {
      status: "authenticated",
      identity: {
        userId,
        email: `${userId}@dev.local`,
        projectScope: env.SCAFFOLD_PROJECT_SCOPE,
        capabilities,
        credential: "dev"
      }
    };
  }
  return verifyScaffoldTokenResult(env, token);
};

export const authenticateScaffold = async (env: ScaffoldAuthEnv, request: Request): Promise<Verified | undefined> => {
  const result = await authenticateScaffoldResult(env, request);
  return result.status === "authenticated" ? result.identity : undefined;
};
