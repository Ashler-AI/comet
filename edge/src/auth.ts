/**
 * Authenticates Scaffold control-plane bearers without handling Google/IAP
 * assertions in Comet. Scaffold's IAP-protected OAuth authorize route binds a
 * bearer to an internal principal. HTTP and socket authorization rendezvous in
 * an AUTH_GRANTS coordinator before fresh `/api/code-sandboxes/auth/session`
 * checks, sharing pending work across isolates but never completed authorization.
 */
import type { Env } from "./env";
import { authenticateDeviceToken, verifyScaffoldThroughCoordinator } from "./auth-routes";
import { boundedAuthJson, scaffoldTokenFits, SCAFFOLD_JSON_BYTES } from "./auth-envelope";

export type ScaffoldAuthEnv = Omit<
  Pick<
    Env,
    | "AUTH_MODE"
    | "ENVIRONMENT"
    | "SCAFFOLD_CONTROL_PLANE_URL"
    | "SCAFFOLD_PROJECT_SCOPE"
    | "SCAFFOLD_REQUIRED_CAPABILITIES"
    | "AUTH_GRANTS"
    | "SCAFFOLD_DEPLOYMENT_ID"
  >,
  "SCAFFOLD_REQUIRED_CAPABILITIES"
> & {
  readonly SCAFFOLD_REQUIRED_CAPABILITIES: string;
};

export interface Verified {
  readonly userId: string;
  readonly email: string;
  readonly projectScope: string;
  readonly orgId?: string;
  readonly deploymentId?: string;
  readonly sessionId?: string;
  readonly targetDeviceId?: string;
  readonly grantId?: string;
  readonly expiresAt?: number;
  readonly lifecycleEpoch?: number;
  readonly sandboxId?: string;
  readonly capabilities: readonly string[];
  readonly credential: "scaffold" | "device" | "dev";
}

export type ScaffoldAuthenticationResult =
  | { status: "authenticated"; identity: Verified }
  | { status: "invalid" }
  | { status: "unavailable"; retryAfter?: string };

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
  "remote_code:exec": ["session.control", "session.environment", "session.sync.write"],
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

// ponytail: retain the measured 2,918 ms attempt budget; the shared 8-second
// envelope also bounds coordinator dispatch, queueing and response decoding.
const SCAFFOLD_AUTH_ATTEMPT_TIMEOUT_MS = 2_918;
const verifyScaffoldTokenResult = async (env: ScaffoldAuthEnv, token: string, signal?: AbortSignal): Promise<ScaffoldAuthenticationResult> => {
  if (!token.startsWith("sc_rc_")) return { status: "invalid" };
  return verifyScaffoldThroughCoordinator(env, token, signal);
};

/** Called only by the internal grant coordinator with its own trusted bindings. */
export const verifyScaffoldSessionResult = async (
  env: ScaffoldAuthEnv, token: string, signal: AbortSignal
): Promise<ScaffoldAuthenticationResult> => {
  if (!scaffoldTokenFits(token)) return { status: "unavailable", retryAfter: "1" };
  if (!token.startsWith("sc_rc_")) return { status: "invalid" };
  const resource = normalizedOrigin(env.SCAFFOLD_CONTROL_PLANE_URL);
  const projectScope = env.SCAFFOLD_PROJECT_SCOPE.trim();
  const required = requiredCapabilities(env);
  if (!resource || !projectScope || required.length === 0) return { status: "unavailable" };

  let session: ScaffoldSession | undefined;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    if (signal.aborted) return { status: "unavailable", retryAfter: "1" };
    const controller = new AbortController();
    const abort = () => { controller.abort(); };
    signal.addEventListener("abort", abort, { once: true });
    const timer = setTimeout(abort, SCAFFOLD_AUTH_ATTEMPT_TIMEOUT_MS);
    try {
      const response = await fetch(`${resource}/api/code-sandboxes/auth/session`, {
        headers: { authorization: `Bearer ${token}`, accept: "application/json" },
        redirect: "manual",
        signal: controller.signal
      });
      if (controller.signal.aborted) {
        await response.body?.cancel();
        controller.signal.throwIfAborted();
      }
      if (!response.ok) {
        await response.body?.cancel();
        // Only explicit credential rejection revokes state. Preserve authority
        // Retry-After exactly, and never amplify throttling with another attempt.
        if (response.status === 401 || response.status === 403) return { status: "invalid" };
        const retryAfter = response.headers.get("retry-after");
        if (retryAfter !== null) {
          if (retryAfter.length > SCAFFOLD_JSON_BYTES || new TextEncoder().encode(retryAfter).byteLength > SCAFFOLD_JSON_BYTES) {
            return { status: "unavailable", retryAfter: "1" };
          }
          return { status: "unavailable", retryAfter };
        }
        if (attempt === 0 && (response.status === 408 || response.status >= 500)) continue;
        return { status: "unavailable" };
      }
      try {
        session = await boundedAuthJson<ScaffoldSession>(response, controller.signal);
      } catch {
        return controller.signal.aborted ? { status: "unavailable", retryAfter: "1" } : { status: "unavailable" };
      }
      break;
    } catch {
      if (signal.aborted) return { status: "unavailable", retryAfter: "1" };
      if (attempt === 1) return controller.signal.aborted ? { status: "unavailable", retryAfter: "1" } : { status: "unavailable" };
    } finally {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
    }
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
      orgId: projectScope,
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
        orgId: env.SCAFFOLD_PROJECT_SCOPE,
        capabilities,
        credential: "dev"
      }
    };
  }
  return verifyScaffoldTokenResult(env, token, request.signal);
};

export const authenticateScaffold = async (env: ScaffoldAuthEnv, request: Request): Promise<Verified | undefined> => {
  const result = await authenticateScaffoldResult(env, request);
  return result.status === "authenticated" ? result.identity : undefined;
};

export const authenticateResult = async (env: Env, request: Request): Promise<ScaffoldAuthenticationResult> => {
  if (!credentialTransportAllowed(request.url)) return { status: "invalid" };
  if (!env.SCAFFOLD_DEPLOYMENT_ID || !env.SCAFFOLD_PROJECT_SCOPE || !env.SCAFFOLD_CONTROL_PLANE_URL) {
    return { status: "unavailable" };
  }
  const token = bearerFromRequest(request);
  if (token?.startsWith("cs1.")) {
    try {
      const identity = await authenticateDeviceToken(env, token);
      return identity ? { status: "authenticated", identity } : { status: "invalid" };
    } catch {
      return { status: "unavailable" };
    }
  }
  const result = await authenticateScaffoldResult(env, request);
  if (result.status === "authenticated") {
    return { status: "authenticated", identity: { ...result.identity, deploymentId: env.SCAFFOLD_DEPLOYMENT_ID } };
  }
  return result;
};
