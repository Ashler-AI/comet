/**
 * Zeron-native edge Worker (design §2, ARCHITECTURE §6): JWT auth at the
 * edge, then forwarding into per-session, per-workspace, and per-device
 * Durable Objects. Also serves content-addressed R2 attachments (§1.2) and
 * the absorbed WorkOS auth routes (formerly apps/server).
 *
 * Crew build (docs/crew/zgx-thin-port.md): crew/router.ts answers first, with
 * Scaffold identities and scoped device grants in place of WorkOS, and hands
 * on only authorized, trusted requests. Of the routes below, this build keeps
 * /preview, /chat2, /registry, /device and /blob. WorkOS and /auth/*, the
 * user directory, usernames, cards, onboarding, plugin keys, the vault,
 * Cloud, runners, /admin, billing, the hosted model list, the public
 * install surface and caller-device proof (device-link.ts) stay in the tree
 * unrouted; the legacy session and workspace lanes answer 410.
 *
 * Routes:
 *   GET  /health
 *   POST /auth/exchange               — WorkOS code → tokens
 *   POST /auth/refresh                — WorkOS refresh → fresh tokens
 *   GET  /auth/orgs                   — caller's active org memberships
 *   POST /auth/orgs                   — create org + admin membership
 *   POST /auth/orgs/:orgId/invitations — admin invites addresses
 *   GET|PATCH /auth/me                — the caller's name and email
 *   PATCH /auth/orgs/:orgId           — admin renames the org
 *   GET  /auth/orgs/:orgId/members    — members (membership ids, never user ids)
 *   PATCH|DELETE /auth/orgs/:orgId/members/:membershipId — admin: role / remove
 *   GET  /auth/orgs/:orgId/invitations — admin: pending + recently expired
 *   DELETE /auth/orgs/:orgId/invitations/:id, POST …/:id/resend — admin
 *   GET  /auth/cli/callback           — headless sign-in paste-code page
 *   GET  /session/:chatId/ws          — loro-protocol room (wss upgrade)
 *   GET  /tail/:chatId                — L2 instant-open tail JSON (§5)
 *   GET  /diff/:chatId                — latest working-tree diff (§6.1)
 *   POST /diff/:chatId                — host publishes the diff sidecar
 *   GET  /snapshot/:chatId            — repair: read current doc snapshot
 *   POST /append/:chatId              — repair: merge-import a Loro update
 *   GET  /workspace/:orgId/ws         — workspace-doc room `ws/{orgId}` (wss; legacy clients)
 *   GET  /workspace/:orgId/tail       — workspace-doc tail JSON
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
 *   PUT|GET|HEAD|DELETE /blob/:chatId/attachments/:uploadId — attachment bytes
 *                                       awaiting a Cloud machine (attachment-blobs.ts)
 *   GET  /chat2/:chatId/ws            — chat2 log-relay room (wss, chat2-sync B)
 *   GET|POST /chat2/:chatId/checkpoint — client-built doc snapshot (Range-resumable GET)
 *   GET|PUT  /chat2/:chatId/tail      — host-published sidecars, served verbatim
 *   GET|PUT  /chat2/:chatId/diff
 *   GET  /chat2/:chatId/stats
 *   POST /chat2/:chatId/reset
 *
 * Cloud device + credential vault (docs/design/cloud-device.md):
 *   POST /runner/enroll | /runner/token — pre-bearer runner identity
 *   POST /runner/heartbeat              — runner bearer
 *   GET|DELETE /cloud/:orgId, POST /cloud/:orgId/{enable,wake,sleep},
 *   GET  /cloud/:orgId/usage            — user bearer
 *   *    /vault/:orgId/…                — VAULT service binding
 *   GET  /admin/cloud/usage             — ADMIN_TOKEN (operator export)
 *   POST /admin/cloud/credits           — ADMIN_TOKEN (grant a user credits)
 *   POST /admin/cloud/machine           — ADMIN_TOKEN (a user's machine size)
 *   POST /admin/cloud/limits            — ADMIN_TOKEN (a user's awake/chat caps)
 *   POST /stripe/webhook | GET /billing/done — pre-bearer billing (Stripe-signed / static page)
 *
 * User directory (user-directory.ts):
 *   POST /workos/webhook                — pre-bearer, WorkOS-signed user, org and membership events
 *   GET  /admin/users[?format=csv]      — ADMIN_TOKEN (every user's profile + join date)
 *   GET  /admin/organizations[?format=csv] — ADMIN_TOKEN (every org with its members)
 *   GET|POST /admin/badges              — ADMIN_TOKEN (list / define badges)
 *   POST /admin/badges/{grant|revoke}   — ADMIN_TOKEN (give or take a badge from users)
 *   POST /admin/usernames/{set|clear|face} — ADMIN_TOKEN (give or take a username; pick a card's scene and foil)
 *
 * Member cards (card-routes.ts):
 *   GET  /cards/:username               — pre-bearer, a card as JSON
 *   GET  /c/:username, /@:username      — pre-bearer, a card's page (also routed from zeron.sh)
 *     (every public card route goes through publicCardGate: per-IP limit, one plain 503)
 *   GET|POST /username, GET /username/check — user bearer (claim, availability)
 */
import { crewRoute } from "./crew/router";
import { AuthGrant } from "./crew/auth-routes";
import { chatRoute } from "./chat-route";
import { deviceParam, forward } from "./forward";
import type { Env } from "./env";
import { SessionRoom } from "./session-room";
import { previewRoute } from "./preview-route";
import { PreviewRoom } from "./preview-room";
import { DeviceRoom } from "./device-room";
import { RegistryRoom } from "./registry-room";
import { ChatRoom } from "./chat-room";
import { CloudAccount } from "./cloud/cloud-account";
import { CloudIndex } from "./cloud/cloud-index";
import { DeleteWorkflow, ProvisionWorkflow, SleepWorkflow, WakeWorkflow } from "./cloud/workflows";
import { runCloudCron } from "./cloud/billing";
import { handleAttachmentBlob } from "./attachment-blobs";
import { runnerChatGate, runnerDeviceGate } from "./runner-access";
import { UserDirectory } from "./user-directory";

export { AuthGrant, SessionRoom, DeviceRoom, RegistryRoom, ChatRoom, PreviewRoom, CloudAccount, CloudIndex, UserDirectory };
export { ProvisionWorkflow, WakeWorkflow, SleepWorkflow, DeleteWorkflow };

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
    headers: { "content-type": "application/json" }
  });

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // Crew: authenticate, authorize and answer Crew's own routes; only a
    // trusted request for the room routing below comes back.
    const crew = await crewRoute(request, env);
    if (crew instanceof Response) return crew;
    const { auth } = crew;
    request = crew.request;
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    const preview = previewRoute(request, env, auth);
    if (preview) return preview;

    // ── session rooms: the legacy Loro lanes, retired (crew/router.ts: 410) ──

    // ── chat2 rooms (docs/chat2-sync.md B): dumb log relays, one per chat.
    //    Claim-on-first-join ownership enforced in the DO (chat ids are
    //    client-minted). The DO handles /ws, /checkpoint (GET Range-resumable
    //    + POST floor-guarded), host-published /tail + /diff sidecars,
    //    /stats, /reset. ──────────────────────────────────────────────────────
    if (parts[0] === "chat2" && parts[1] && ID_RE.test(parts[1])) {
      // A Cloud runner reaches only the chats its device hosts.
      const notHost = await runnerChatGate(env, auth, parts[1]);
      if (notHost) return notHost;
    }
    // Crew: no org header, so a chat room never asks the registry who hosts
    // it (`/chat-host`); only the relay layer, off in this build, would.
    const chatResponse = chatRoute(request, env, auth.userId);
    if (chatResponse) return chatResponse;

    // ── workspace rooms: the legacy Loro workspace doc, retired (crew/router.ts: 410)

    // ── registry rooms (docs/registry-sync.md): the row-table replacement for
    //    the Loro workspace doc. Same trust shape as /workspace: org claim
    //    must match the URL, room derived from the caller's OWN user id, DO
    //    trusts the stamped header. `reg1` = first registry generation. ─────
    if (parts[0] === "registry" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      // Crew: the org is the Scaffold project (a device credential carries no org claim).
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
          auth,
          "/ws",
          `?${deviceParam(url).replace(/^&/, "")}`
        );
      }
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth, "/stats", "");
      }
      // Pull over plain HTTPS: `?since=` returns the same delta the WS
      // hello would (full table without it — the original repair read).
      // One round trip on any network that passes HTTPS at all, where the
      // WS upgrade needs 4 and a cooperative middlebox.
      if (parts[2] === "rows" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth, "/rows", url.search);
      }
      // Push over plain HTTPS — the WS push's fallback twin (LWW clocks
      // make replays no-ops, so at-least-once delivery is safe).
      if (parts[2] === "push" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth, "/push", url.search);
      }
      // A phone's APNs token + notification choices (POST), or removal on
      // sign-out / turning notifications off (DELETE). `?device=` required.
      if (parts[2] === "push-target" && (request.method === "POST" || request.method === "DELETE")) {
        return forward(env.REGISTRY_ROOMS, room, request, auth, "/push-target", url.search);
      }
      // Operator wipe. Unlike the CRDT rooms this needs no recipe: clients
      // detect the seq regression on their next hello and re-seed the table
      // from local rows with original clocks, automatically.
      if (parts[2] === "reset" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth, "/reset", "");
      }
    }

    // ── device rooms ────────────────────────────────────────────────────────
    if (parts[0] === "device" && parts[1] && ID_RE.test(parts[1])) {
      const deviceId = parts[1];
      // Another device's link or nudge: only a delegate's machine (the
      // policy already admitted nothing else but liveness).
      if (parts[2] === "ws" || parts[2] === "nudge") {
        const refusedDevice = await runnerDeviceGate(env, auth, deviceId);
        if (refusedDevice) return refusedDevice;
      }
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        // Crew: dialDeviceRoom's caller-device proof needs plugin keys, which
        // stay unrouted here, so no client dial is stamped (`fromDevice`).
        const role = url.searchParams.get("role") === "host" ? "host" : "client";
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        return forward(
          env.DEVICE_ROOMS,
          `d2/${deviceId}`,
          request,
          auth,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}`
        );
      }
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth, `/sidecar/${parts[3]}`, "");
      }
      if (parts[2] === "status") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth, "/status", "");
      }
      // Durable command nudge (§7): "chat X has pending commands — open its
      // doc". Delivered live if the host is connected, else queued in the DO
      // and replayed on the host's next join.
      if (parts[2] === "nudge" && request.method === "POST") {
        // (Crew: no Cloud machines to wake; the nudge queues as for any host.)
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth, "/nudge", "");
      }
    }

    // ── attachment bytes on their way to a Cloud machine (attachment-blobs.ts).
    if (parts[0] === "blob" && parts.length === 4 && parts[2] === "attachments" && ID_RE.test(parts[1]) && ID_RE.test(parts[3])) {
      return handleAttachmentBlob(request, env, auth, parts[1], parts[3]);
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
      const notHost = await runnerChatGate(env, auth, parts[1]);
      if (notHost) return notHost;
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

    // ── retired attachment mirror (clients ≤0.1.62): crew/router.ts answers
    //    410, never acknowledging bytes it discards ──────────────────────────

    return json({ error: "not_found" }, 404);
  },

  /** Hourly (`17 * * * *`): Cloud usage reconcile backstop, daily orphan
   * scan, closed-month exports (cloud/billing.ts). */
  scheduled(controller: ScheduledController, env: Env, ctx: ExecutionContext): void {
    ctx.waitUntil(runCloudCron(env, controller.scheduledTime));
  }
} satisfies ExportedHandler<Env>;
