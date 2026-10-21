/**
 * Crew upstream canary: Scaffold-verified personal registry, opaque chat2
 * transport and capability-scoped DeviceRoom relay. No legacy workspace writes.
 *
 * Identity routes (all resource-bound to the configured Scaffold origin):
 *   GET  /health
 *   GET  /auth/session
 *   POST|DELETE /auth/device-grants
 *   POST /auth/device-grants/exchange — one-time grant, public bearer exchange
 *   POST /auth/device-token/refresh   — rotates an unexpired device credential
 *   GET|HEAD /releases/*             — authenticated isolated R2 release artifacts
 * Sync routes:
 *   GET  /registry/:orgId/ws          — workspace registry room `reg1/{orgId}/{user}` (wss)
 *   GET  /registry/:orgId/stats       — registry seq/rows/attribution
 *   GET  /registry/:orgId/rows        — registry full-table repair read
 *   POST /registry/:orgId/reset       — registry operator wipe (self-healing)
 *   GET  /device/:deviceId/ws?role=   — device-room byte pipe (§8)
 *   GET  /device/:deviceId/sidecar/:name
 *   POST /device/:deviceId/sidecar/:name
 *   GET  /device/:deviceId/status
 *   PUT  /blob/:chatId/:partId        — tool-output sidecar (chat2-sync A2)
 *   GET  /blob/:chatId/:partId
 *   GET  /chat2/:chatId/ws            — chat2 log-relay room (wss, chat2-sync B)
 *   GET|POST /chat2/:chatId/checkpoint — client-built doc snapshot (Range-resumable GET)
 *   GET|PUT  /chat2/:chatId/tail      — host-published sidecars, served verbatim
 *   GET|PUT  /chat2/:chatId/diff
 *   GET  /chat2/:chatId/stats
 *   POST /chat2/:chatId/reset
 */
import { authenticateResult } from "./auth";
import { authorizeRoute, trustedRequest } from "./authority";
import { chatRoute } from "./chat-route";
import { AuthGrant, handlePublicAuthRoute, handleAuthenticatedAuthRoute } from "./auth-routes";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";
import { SessionRoom } from "./session-room";
import { previewRoute } from "./preview-route";
import { PreviewRoom } from "./preview-room";
import { DeviceRoom } from "./device-room";
import { RegistryRoom } from "./registry-room";
import { ChatRoom } from "./chat-room";
import { peerRoute } from "./peer-route";

export { AuthGrant, SessionRoom, DeviceRoom, RegistryRoom, ChatRoom, PreviewRoom };

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

/** `decodeURIComponent` that answers `undefined` for malformed %-escapes. */
const safeDecode = (segment: string): string | undefined => {
  try {
    return decodeURIComponent(segment);
  } catch {
    return undefined;
  }
};
/** Tool part ids are harness-minted (`tool-1`, `call_x`, `m1#c1`-style) —
 * wider than ID_RE but still no slashes, so a part id can't traverse keys. */
const PART_RE = /^[A-Za-z0-9._:#~-]{1,200}$/;
const MAX_TOOL_BLOB_BYTES = 1024 * 1024;

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" }
  });

/** Forward into a DO with the verified user stamped on the request. */
const forward = (
  ns: DurableObjectNamespace,
  name: string,
  request: Request,
  userId: string,
  path: string,
  search?: string
): Promise<Response> => {
  const stub = ns.get(ns.idFromName(name));
  const url = new URL(request.url);
  url.pathname = path;
  if (search !== undefined) url.search = search;
  url.searchParams.delete("token");
  const headers = new Headers(request.headers);
  // Never forward caller-selected room authority.
  headers.delete(ROOM_KIND_HEADER);
  headers.set(AUTH_USER_HEADER, userId);
  return stub.fetch(new Request(url.toString(), { ...requestInit(request), headers }));
};

const requestInit = (request: Request): RequestInit => ({
  method: request.method,
  body: request.body
});

/** Carry the dialing engine's `&device=` through to the DO (socket
 * attribution in logs — the 2026-08-04 deaf socket was only identifiable by
 * reverse-engineering rotating IPv6 privacy addresses). Validated so a
 * hand-crafted value can't inject into log lines or the DO's query. */
const deviceParam = (url: URL): string => {
  const device = url.searchParams.get("device") ?? "";
  return ID_RE.test(device) ? `&device=${device}` : "";
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (url.pathname === "/health") {
      return json({ ok: true, auth: env.AUTH_MODE, environment: env.ENVIRONMENT });
    }

    // This isolated generation has no public upstream installer or release feed.
    if (url.pathname === "/install.sh") return json({ error: "not_found" }, 404);
    try {
      const publicAuth = await handlePublicAuthRoute(request, env, url);
      if (publicAuth) return publicAuth;
    } catch {
      return json({ error: "authority_unavailable" }, 503);
    }
    const result = await authenticateResult(env, request);
    if (result.status !== "authenticated") {
      return json({ error: result.status === "invalid" ? "unauthenticated" : "authority_unavailable" },
        result.status === "invalid" ? 401 : 503);
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
    // Compatibility is ownership-only: never revive the legacy session sync lane.
    if (parts[0] === "session" && parts[2] === "directory-authority" && parts.length === 3) {
      if (request.method !== "GET") return json({ error: "method_not_allowed" }, 405);
      if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(parts[1])) {
        return json({ error: "invalid_session" }, 400);
      }
      if (!auth.capabilities.includes("session.control") ||
          !auth.deploymentId || url.searchParams.get("deploymentId") !== auth.deploymentId) {
        return json({ error: "forbidden" }, 403);
      }
      try {
        const response = await forward(env.CHAT_ROOMS, `chat2/${parts[1]}`, request, auth.userId, "/directory-authority", "");
        if (!response.ok) return json({ error: "directory_authority_unavailable" }, 503);
        const result = await response.json() as { ownsSession?: unknown };
        if (typeof result.ownsSession !== "boolean") return json({ error: "directory_authority_unavailable" }, 503);
        return json({ ownsSession: result.ownsSession, projectId: auth.projectScope, actorId: auth.email, subject: auth.userId });
      } catch {
        return json({ error: "directory_authority_unavailable" }, 503);
      }
    }
    // Legacy document lanes cannot share this generation's ownership or grants.
    if (["session", "tail", "stats", "diff", "snapshot", "append", "workspace"].includes(parts[0])) {
      return json({ error: "use_registry_chat2" }, 410);
    }
    if (
      ((parts[0] === "releases" && parts.length >= 2) ||
        (parts[0] === "api" && parts[1] === "releases" && parts.length === 3)) &&
      (request.method === "GET" || request.method === "HEAD")
    ) {
      const key = safeDecode(parts[0] === "api" ? parts[2] : url.pathname.slice("/releases/".length));
      if (!key || key.includes("..")) return json({ error: "bad request" }, 400);
      const object = await env.RELEASES.get(key);
      if (!object) return json({ error: "not_found" }, 404);
      // latest.txt / manifest.json flip on release; artifacts are immutable by name.
      const mutable = key.endsWith(".txt") || key.endsWith(".json");
      const headers = new Headers({
        "content-type": key.endsWith(".txt")
          ? "text/plain; charset=utf-8"
          : key.endsWith(".json")
            ? "application/json"
            : "application/octet-stream",
        "content-length": String(object.size),
        "cache-control": mutable ? "private, max-age=60" : "private, max-age=86400, immutable",
        etag: object.httpEtag
      });
      return new Response(request.method === "HEAD" ? null : object.body, { headers });
    }


    const preview = previewRoute(request, env, auth);
    if (preview) return preview;

    // ── chat2 rooms (docs/chat2-sync.md B): dumb log relays, one per chat.
    //    Claim-on-first-join ownership enforced in the DO (chat ids are
    //    client-minted). The DO handles /ws, /checkpoint (GET Range-resumable
    //    + POST floor-guarded), host-published /tail + /diff sidecars,
    //    /stats, /reset. ──────────────────────────────────────────────────────
    const chatResponse = chatRoute(request, env, auth.userId);
    if (chatResponse) return chatResponse;


    // ── registry rooms (docs/registry-sync.md): the row-table replacement for
    //    the Loro workspace doc. Same trust shape as /workspace: org claim
    //    must match the URL, room derived from the caller's OWN user id, DO
    //    trusts the stamped header. `reg1` = first registry generation. ─────
    if (parts[0] === "registry" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      if ((auth.credential === "device" ? auth.projectScope : auth.orgId) !== orgId) return json({ error: "forbidden" }, 403);
      const room = `reg1/${orgId}/${auth.userId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        return forward(
          env.REGISTRY_ROOMS,
          room,
          request,
          auth.userId,
          "/ws",
          `?${deviceParam(url).replace(/^&/, "")}`
        );
      }
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/stats", "");
      }
      // Pull over plain HTTPS: `?since=` returns the same delta the WS
      // hello would (full table without it — the original repair read).
      // One round trip on any network that passes HTTPS at all, where the
      // WS upgrade needs 4 and a cooperative middlebox.
      if (parts[2] === "rows" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/rows", url.search);
      }
      // Push over plain HTTPS — the WS push's fallback twin (LWW clocks
      // make replays no-ops, so at-least-once delivery is safe).
      if (parts[2] === "push" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/push", url.search);
      }
      // A phone's APNs token + notification choices (POST), or removal on
      // sign-out / turning notifications off (DELETE). `?device=` required.
      if (parts[2] === "push-target" && (request.method === "POST" || request.method === "DELETE")) {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/push-target", url.search);
      }
      // Operator wipe. Unlike the CRDT rooms this needs no recipe: clients
      // detect the seq regression on their next hello and re-seed the table
      // from local rows with original clocks, automatically.
      if (parts[2] === "reset" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/reset", "");
      }
    }

    // ── device rooms ────────────────────────────────────────────────────────
    if (parts[0] === "device" && parts[1] && ID_RE.test(parts[1])) {
      const deviceId = parts[1];
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        const role = url.searchParams.get("role") === "host" ? "host" : "client";
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        // `d2/` — same staging→prod identity break as `s2/` above.
        return forward(
          env.DEVICE_ROOMS,
          `d2/${deviceId}`,
          request,
          auth.userId,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}`
        );
      }
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, `/sidecar/${parts[3]}`, "");
      }
      if (parts[2] === "status") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/status", "");
      }
      // Durable command nudge (§7): "chat X has pending commands — open its
      // doc". Delivered live if the host is connected, else queued in the DO
      // and replayed on the host's next join.
      if (parts[2] === "nudge" && request.method === "POST") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/nudge", "");
      }
    }

    // ── R2 tool-output sidecar (docs/chat2-sync.md A2): full tool outputs
    //    and diffs live here, keyed `{chatId}/{partId}[.diff]`; the doc keeps
    //    only a one-line summary + this key. Straight R2, no DO involvement —
    //    the doc stays thin whether or not these uploads land. Per-user
    //    prefix = owner auth. ─────────────────────────────────────────────
    if (parts[0] === "blob" && parts.length === 3 && ID_RE.test(parts[1])) {
      // Percent-decode the part segment before validating: PART_RE allows
      // `#` (`m1#c1`-style harness ids), which HTTP clients cannot send raw
      // (fragment delimiter) — the host percent-encodes it. Decode-then-
      // validate keeps traversal shut: `%2F` decodes to `/`, fails PART_RE.
      const partId = safeDecode(parts[2]);
      if (partId === undefined || !PART_RE.test(partId)) {
        return json({ error: "bad part id" }, 400);
      }
      const key = `blob/${auth.userId}/${parts[1]}/${partId}`;
      if (request.method === "PUT") {
        const body = await request.arrayBuffer();
        // Outputs are 4KiB-capped at the harness boundary; diffs can run
        // larger but a sidecar entry is one tool result, never a dump.
        if (body.byteLength > MAX_TOOL_BLOB_BYTES) return json({ error: "too_large" }, 413);
        await env.BLOBS.put(key, body, {
          httpMetadata: {
            contentType: request.headers.get("content-type") ?? "text/plain; charset=utf-8"
          }
        });
        return json({ ok: true, bytes: body.byteLength });
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object =
          request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
        if (!object) return json({ error: "not_found" }, 404);
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set("etag", object.httpEtag);
        // Re-resolved tool parts overwrite their key, so short-lived caching only.
        headers.set("cache-control", "private, max-age=300");
        const body =
          request.method === "GET" && "body" in object ? (object as R2ObjectBody).body : null;
        return new Response(body, { headers });
      }
    }

    // ── retired attachment mirror (clients ≤0.1.62): acknowledge and discard
    //    so old outboxes drain once instead of retrying the PUT forever ──────
    if (parts[0] === "attachments" && parts[1] && request.method === "PUT") {
      return json({ ok: true });
    }

    return json({ error: "not_found" }, 404);
  }
} satisfies ExportedHandler<Env>;
