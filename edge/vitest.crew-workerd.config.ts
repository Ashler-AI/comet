import { defineConfig } from "vitest/config";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { Response as MiniflareResponse } from "miniflare";

// Crew's runtime-real tier (docs/crew/zgx-thin-port.md): upstream's rooms with
// Crew's grafts and Crew's AuthGrant inside workerd, against a fake Scaffold
// authority. `npm run test:crew-workerd`. Nothing here leaves the process:
// every outbound fetch lands in the handler below or fails the test.
//
// This pool version has no cloudflare:test fetchMock. Intercept only network
// fetches here; consumer-to-AuthGrant requests still cross real DO bindings.
const authorityReplies = new Map<string, { status: number; retryAfter?: string; scopes?: string[]; requests: Array<{ at: number; url: string }> }>();

export default defineConfig({
  plugins: [
    cloudflareTest({
      main: "./test/crew-workerd/fixture.ts",
      miniflare: {
        compatibilityDate: "2026-07-01",
        compatibilityFlags: ["nodejs_compat"],
        outboundService: async (request) => {
          const url = new URL(request.url);
          if (url.origin === "https://authority-test.internal") {
            const token = url.searchParams.get("token")!;
            if (request.method === "POST") {
              const reply = await request.json() as { status: number; retryAfter?: string; scopes?: string[] };
              authorityReplies.set(token, { ...reply, requests: authorityReplies.get(token)?.requests ?? [] });
            } else if (request.method === "DELETE") {
              authorityReplies.delete(token);
            }
            return MiniflareResponse.json(authorityReplies.get(token) ?? null);
          }
          const token = request.headers.get("authorization")?.replace(/^Bearer /, "");
          const reply = token && authorityReplies.get(token);
          if (!reply) throw new Error(`Unexpected outbound authority request: ${url.origin}${url.pathname}`);
          reply.requests.push({ at: Date.now(), url: request.url });
          if (request.method !== "GET" || url.pathname !== "/api/code-sandboxes/auth/session") {
            return new MiniflareResponse("Unexpected authority operation", { status: 400 });
          }
          return MiniflareResponse.json({
            ok: true, actor: { sub: "owner@example.com", auth: "iap" },
            resource: "https://authority.example", scopes: reply.scopes ?? ["remote_code:read"]
          }, { status: reply.status, headers: reply.retryAfter === undefined ? {} : { "retry-after": reply.retryAfter } });
        },
        // Real-clock reuse bounds, short enough to cross in a test (production defaults: 10 min / 24 h).
        bindings: { AUTH_MODE: "dev", ENVIRONMENT: "local", SCAFFOLD_PROJECT_SCOPE: "canary", SCAFFOLD_DEPLOYMENT_ID: "canary", SCAFFOLD_REQUIRED_CAPABILITIES: "session.read", SCAFFOLD_CONTROL_PLANE_URL: "https://authority.example", SCAFFOLD_SESSION_FRESH_MS: "300", SCAFFOLD_SESSION_GRACE_MS: "2000" },
        r2Buckets: ["BLOBS"],
        durableObjects: {
          AUTH_GRANTS: { className: "AuthGrant", useSQLite: true },
          DEVICE_ROOMS: { className: "DeviceRoom", useSQLite: true },
          TEST_LOG: { className: "TestLogRoom", useSQLite: true },
          CHAT_ROOMS: { className: "ChatRoom", useSQLite: true },
          PREVIEW_ROOMS: { className: "PreviewRoom", useSQLite: true },
          REGISTRY_ROOMS: { className: "RegistryRoom", useSQLite: true }
        }
      }
    })
  ],
  resolve: {
    // Mirror wrangler.jsonc: workerd cannot fetch loro's WASM by URL; the
    // base64 entry inlines it.
    alias: { "loro-crdt": "loro-crdt/base64" }
  },
  test: {
    // Each worker owns a Miniflare/SQLite runtime; bound resources while keeping files parallel.
    maxWorkers: 2,
    include: ["test/crew-workerd/**/*.test.ts"]
  }
});
