import { chatRoute } from "../../src/chat-route";
import { previewRoute } from "../../src/preview-route";
import { authenticateResult } from "../../src/auth";
import { requestAuthority, sendAuthorized, socketAuthorized, trustedRequest } from "../../src/authority";
import type { Env } from "../../src/env";
export { DeviceRoom } from "../../src/device-room";
export { ChatRoom } from "../../src/chat-room";
export { PreviewRoom } from "../../src/preview-room";
export { RegistryRoom } from "../../src/registry-room";
export { AuthGrant } from "../../src/auth-routes";
import { DurableObject } from "cloudflare:workers";

/** SQLite-backed test consumer; exercises the real authority helpers and DO bindings. */
export class TestLogRoom extends DurableObject<Env> {
  async fetch(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    const authorityEnv = { ...this.env, AUTH_MODE: "scaffold" };
    if (path === "/authority/check") {
      const overrides = await request.json<Partial<Env>>();
      return Response.json(await authenticateResult({ ...authorityEnv, ...overrides }, request));
    }
    if (path === "/authority/ws") {
      const result = await authenticateResult(authorityEnv, request);
      if (result.status !== "authenticated") return Response.json(result, { status: 401 });
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      pair[1].serializeAttachment({ authority: requestAuthority(trustedRequest(request, result.identity)) });
      return new Response(null, { status: 101, webSocket: pair[0] });
    }
    if (path === "/authority/send") {
      const frames = await request.json<string[]>();
      for (const ws of this.ctx.getWebSockets()) {
        for (const frame of frames) sendAuthorized(authorityEnv, this.ctx, ws, frame);
      }
      return new Response(null, { status: 204 });
    }
    return new Response("Not found", { status: 404 });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (!(await socketAuthorized({ ...this.env, AUTH_MODE: "scaffold" }, ws))) return;
    await this.ctx.storage.put("authorizedMessage", message);
  }

  async webSocketClose(ws: WebSocket, code: number, reason: string): Promise<void> {
    ws.close(code, reason);
    await this.ctx.storage.put("socketClosed", true);
  }
}

export default {
  fetch(request: Request, env: { PREVIEW_ROOMS: DurableObjectNamespace; CHAT_ROOMS: DurableObjectNamespace }): Response | Promise<Response> {
    // Test credentials exercise the production routing seam without loading
    // the unrelated session-room WASM inside the Workers test runner.
    const bearer = request.headers.get("authorization");
    if (!bearer?.startsWith("Bearer ")) return new Response("Unauthorized", { status: 401 });
    const [userId, orgId] = bearer.slice(7).split("@");
    return chatRoute(request, env, userId) ?? previewRoute(request, env, { userId, orgId }) ?? new Response("test fixture", { status: 404 });
  }
};
