/**
 * Crew's front of the Edge Worker (docs/crew/zgx-thin-port.md): everything
 * decided before upstream's room routing in ../index.ts sees a request.
 *
 * - Scaffold bearers (`sc_rc_…`) and sandbox device credentials (`cs1.…`)
 *   replace WorkOS (auth.ts). Only /health and the one-time grant exchange
 *   are public.
 * - Every other request is authenticated, passes authorizeRoute (a device
 *   credential reaches only its own session's typed lanes) and leaves as
 *   trustedRequest: caller-supplied x-crew-*, x-comet-* and x-zeron-* headers
 *   dropped, the bearer removed, the verified authority stamped for the rooms.
 * - Crew's own routes answer here; the legacy Loro lanes and the retired
 *   attachment mirror answer 410. Anything else goes on to index.ts.
 *
 * Answered here:
 *   GET  /health
 *   POST /auth/device-grants/exchange — one-time grant → device credential (no bearer)
 *   GET  /auth/session                — the caller's verified identity
 *   POST|DELETE /auth/device-grants   — issue / revoke a sandbox host's grant
 *   POST /auth/device-token/refresh   — rotate an unexpired device credential
 *   GET  /session/:id/peer/ws, /device/:id/ws?purpose=peer-reply — typed peer relay
 *   GET  /session/:id/directory-authority — chat2 ownership (session.control)
 *   GET|HEAD /releases/:file, /api/releases/:file, /install.sh — private release feed
 *   *    /session, /tail, /stats, /diff, /snapshot, /append, /workspace — 410
 *   *    /attachments/… — 410
 */
import { authenticateResult, type Verified } from "./auth";
import { handleAuthenticatedAuthRoute, handlePublicAuthRoute } from "./auth-routes";
import { authorizeRoute, trustedRequest } from "./authority";
import { peerRoute } from "./peer-route";
import { fetchReleaseObject } from "./release-feed";
import type { Env } from "../env";
import { forward } from "../forward";

const RELEASE_FILE_RE = /^[A-Za-z0-9._-]{1,200}$/;
const SESSION_UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
/** Upstream's Loro document lanes; this generation's ownership and grants never reach them. */
const LEGACY_LANES = ["session", "tail", "stats", "diff", "snapshot", "append", "workspace"];

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" }
  });

/** `decodeURIComponent` that answers `undefined` for malformed %-escapes. */
const safeDecode = (segment: string): string | undefined => {
  try {
    return decodeURIComponent(segment);
  } catch {
    return undefined;
  }
};

/** A request for upstream's room routing: the verified caller, and the
 * trusted request to forward (never the caller's own). */
export interface CrewRouted {
  readonly auth: Verified;
  readonly request: Request;
}

export async function crewRoute(request: Request, env: Env): Promise<Response | CrewRouted> {
  const url = new URL(request.url);
  const parts = url.pathname.split("/").filter(Boolean);

  if (url.pathname === "/health") {
    return json({ ok: true, auth: env.AUTH_MODE, environment: env.ENVIRONMENT });
  }

  try {
    const publicAuth = await handlePublicAuthRoute(request, env, url);
    if (publicAuth) return publicAuth;
  } catch {
    return json({ error: "authority_unavailable" }, 503);
  }
  const result = await authenticateResult(env, request);
  if (result.status !== "authenticated") {
    const response = json({ error: result.status === "invalid" ? "unauthenticated" : "authority_unavailable" },
      result.status === "invalid" ? 401 : 503);
    if (result.status === "unavailable" && result.retryAfter !== undefined) response.headers.set("retry-after", result.retryAfter);
    return response;
  }
  const auth = result.identity;
  if (url.pathname === "/auth/session" && request.method === "GET") return json(auth);
  try {
    const authRoute = await handleAuthenticatedAuthRoute(request, env, url, auth);
    if (authRoute) return authRoute;
  } catch {
    return json({ error: "authority_unavailable" }, 503);
  }
  const peer = await peerRoute(request, env, auth);
  if (peer) return peer;
  if (!authorizeRoute(request, auth)) return json({ error: "forbidden" }, 403);
  request = trustedRequest(request, auth);

  if (url.pathname === "/install.sh") {
    if (!auth.capabilities.includes("session.read") || !["GET", "HEAD"].includes(request.method)) {
      return json({ error: "forbidden" }, 403);
    }
    return fetchReleaseObject(env, "install.sh", request.method as "GET" | "HEAD");
  }
  // Compatibility is ownership-only: never revive the legacy session sync lane.
  if (parts[0] === "session" && parts[2] === "directory-authority" && parts.length === 3) {
    if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
    if (!SESSION_UUID_RE.test(parts[1])) return json({ error: "invalid_session" }, 400);
    if (!auth.capabilities.includes("session.control") ||
        !auth.deploymentId || url.searchParams.get("deploymentId") !== auth.deploymentId) {
      return json({ error: "forbidden" }, 403);
    }
    try {
      const response = await forward(env.CHAT_ROOMS, `chat2/${parts[1]}`, request, auth, "/directory-authority", "");
      if (!response.ok) return json({ error: "directory_authority_unavailable" }, 503);
      const owned = await response.json() as { ownsSession?: unknown };
      if (typeof owned.ownsSession !== "boolean") return json({ error: "directory_authority_unavailable" }, 503);
      return json({ ownsSession: owned.ownsSession, projectId: auth.projectScope, actorId: auth.email, subject: auth.userId });
    } catch {
      return json({ error: "directory_authority_unavailable" }, 503);
    }
  }
  if (LEGACY_LANES.includes(parts[0])) return json({ error: "use_registry_chat2" }, 410);
  if (parts[0] === "releases" || (parts[0] === "api" && parts[1] === "releases")) {
    if (!auth.capabilities.includes("session.read") || !["GET", "HEAD"].includes(request.method)) {
      return json({ error: "forbidden" }, 403);
    }
    const file = safeDecode(parts[0] === "api" ? parts[2] ?? "" : parts[1] ?? "");
    if (parts.length !== (parts[0] === "api" ? 3 : 2) || !file ||
        !RELEASE_FILE_RE.test(file) || file.includes("..")) {
      return json({ error: "bad_request" }, 400);
    }
    return fetchReleaseObject(env, file, request.method as "GET" | "HEAD");
  }
  // The retired mirror cannot persist data in this generation. Never report a
  // successful upload after discarding its bytes.
  if (parts[0] === "attachments") return json({ error: "legacy_attachments_retired" }, 410);

  return { auth, request };
}
