// Crew's seams in upstream files (docs/crew/zgx-thin-port.md): every DO forward
// carries only the authority the Worker verified, and upstream's newer lanes
// sit behind authorizeRoute.
import { describe, expect, it } from "vitest";
import { forward } from "../forward";
import { chatRoute } from "../chat-route";
import { previewRoute } from "../preview-route";
import type { Env } from "../env";
import type { Verified } from "./auth";
import { AUTHORITY_HEADER, authorizeRoute, requestAuthority, trustedRequest } from "./authority";

const owner: Verified = {
  kind: "user", userId: "owner@example.com", email: "owner@example.com", projectScope: "canary", orgId: "canary",
  deploymentId: "canary", capabilities: ["session.read", "session.chat"], credential: "scaffold",
};
const host: Verified = {
  ...owner, orgId: undefined, credential: "device", sessionId: "session-a", targetDeviceId: "device-a",
  capabilities: ["session.read"],
};

/** A namespace whose every stub records the request it was handed. */
const recording = () => {
  const seen: Request[] = [];
  const ns = {
    idFromName: (name: string) => name,
    get: () => ({ fetch: async (request: Request) => { seen.push(request); return new Response(null, { status: 204 }); } }),
  } as unknown as DurableObjectNamespace;
  return { ns, seen };
};

const forged = { "x-crew-authority": JSON.stringify({ userId: "attacker", writable: true }), "x-crew-native-grant-issuer": "1" };

describe("x-crew-* headers on a DO forward", () => {
  const forwarders: Record<string, { path: string; run: (ns: DurableObjectNamespace, request: Request) => Promise<Response> | Response }> = {
    "forward.ts": { path: "/registry/canary/rows", run: (ns, request) => forward(ns, "reg1/canary/owner", request, owner, "/rows", "") },
    "chat-route.ts": { path: "/chat2/chat-a/rows", run: (ns, request) => chatRoute(request, { CHAT_ROOMS: ns }, owner.userId)! },
    "preview-route.ts": { path: "/preview/canary/ws?device=desktop", run: (ns, request) => previewRoute(request, { PREVIEW_ROOMS: ns }, owner)! },
  };

  it.each(Object.keys(forwarders))("%s drops every caller-supplied x-crew-* header from a request the Worker didn't stamp", async name => {
    const { ns, seen } = recording();
    const { path, run } = forwarders[name];
    await run(ns, new Request(`https://edge.example${path}`, { headers: { ...forged, upgrade: "websocket" } }));
    expect(seen).toHaveLength(1);
    expect([...seen[0].headers.keys()].filter(key => key.startsWith("x-crew-"))).toEqual([]);
  });

  it.each(Object.keys(forwarders))("%s keeps exactly the authority trustedRequest stamped, whatever the caller sent", async name => {
    const { ns, seen } = recording();
    const { path, run } = forwarders[name];
    const trusted = trustedRequest(new Request(`https://edge.example${path}`, {
      headers: { ...forged, upgrade: "websocket", authorization: "Bearer sc_rc_owner" },
    }), owner);
    await run(ns, trusted);
    const forwarded = seen[0];
    expect(forwarded.headers.get(AUTHORITY_HEADER)).toBe(trusted.headers.get(AUTHORITY_HEADER));
    expect(requestAuthority(forwarded)?.userId).toBe(owner.userId);
    expect(forwarded.headers.has("x-crew-native-grant-issuer")).toBe(false);
    expect(forwarded.headers.has("authorization")).toBe(false);
  });

  it("never lets a copy of a trusted request inherit its authority", async () => {
    const { ns, seen } = recording();
    const trusted = trustedRequest(new Request("https://edge.example/registry/canary/rows"), owner);
    // Same headers, different Request: the stamp belongs to the object, not its headers.
    await forward(ns, "reg1/canary/owner", new Request(trusted), owner, "/rows", "");
    expect(seen[0].headers.has(AUTHORITY_HEADER)).toBe(false);
  });
});

describe("upstream's attachment lane for a sandbox host", () => {
  const req = (path: string, method = "GET") => new Request(`https://edge.example${path}`, { method });
  const writer: Verified = { ...host, capabilities: ["session.read", "session.sync.write"] };

  it("reads its own session's attachments and writes or collects them only with session.sync.write", () => {
    for (const method of ["GET", "HEAD"]) expect(authorizeRoute(req("/blob/session-a/attachments/u1", method), host), method).toBe(true);
    for (const method of ["PUT", "DELETE"]) {
      expect(authorizeRoute(req("/blob/session-a/attachments/u1", method), host), method).toBe(false);
      expect(authorizeRoute(req("/blob/session-a/attachments/u1", method), writer), method).toBe(true);
    }
    expect(authorizeRoute(req("/blob/session-a/attachments/u1", "POST"), writer)).toBe(false);
  });

  it("never reaches another session's bytes, nor deletes a tool output", () => {
    for (const method of ["GET", "PUT", "DELETE"]) {
      expect(authorizeRoute(req("/blob/session-b/attachments/u1", method), writer), method).toBe(false);
    }
    expect(authorizeRoute(req("/blob/session-a/tool-1", "DELETE"), writer)).toBe(false);
    expect(authorizeRoute(req("/blob/session-a/tool-1", "PUT"), writer)).toBe(true);
    expect(authorizeRoute(req("/blob/session-a/attachments/u1"), { ...writer, capabilities: ["session.sync.write"] })).toBe(false);
  });

  it("closes the lanes this build leaves unrouted to a sandbox host", () => {
    for (const path of ["/username", "/onboarding", "/plugin-keys", "/cloud/canary", "/vault/canary", "/models/v1.json",
      "/auth/orgs", "/registry/canary/chat-host", "/registry/canary/device-parents", "/preview/canary/ws?device=device-a"]) {
      expect(authorizeRoute(req(path), writer), path).toBe(false);
    }
  });
});
