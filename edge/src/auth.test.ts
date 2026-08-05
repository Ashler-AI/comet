import { afterEach, describe, expect, it, vi } from "vitest";
import {
  authenticateScaffold,
  credentialTransportAllowed,
  verifyScaffoldToken,
  type ScaffoldAuthEnv
} from "./auth";

const env = {
  AUTH_MODE: "scaffold",
  ENVIRONMENT: "staging",
  SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold-staging.internal.ashler.com",
  SCAFFOLD_PROJECT_SCOPE: "ashler-staging",
  SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.control"
} satisfies ScaffoldAuthEnv;

afterEach(() => vi.unstubAllGlobals());
describe("credential transport", () => {
  it("allows secure remote and explicit loopback origins only", () => {
    expect(credentialTransportAllowed("https://comet.example/session/x")).toBe(true);
    expect(credentialTransportAllowed("wss://comet.example/device/x/ws")).toBe(true);
    expect(credentialTransportAllowed("http://localhost:8787/session/x")).toBe(true);
    expect(credentialTransportAllowed("ws://127.0.0.1:8787/device/x/ws")).toBe(true);
    expect(credentialTransportAllowed("http://comet.example/session/x")).toBe(false);
    expect(credentialTransportAllowed("ws://comet.example/device/x/ws")).toBe(false);
  });

  it("rejects an authenticated remote HTTP request before credential introspection", async () => {
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    await expect(
      authenticateScaffold(
        env,
        new Request("http://comet.example/session/session-1/ws?token=sc_rc_test")
      )
    ).resolves.toBeUndefined();
    expect(fetch).not.toHaveBeenCalled();
  });
});


describe("Scaffold bearer validation", () => {
  it("maps an IAP principal and operator project scope", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          ok: true,
          resource: "https://scaffold-staging.internal.ashler.com",
          actor: { sub: "Developer@Ashler.ai", auth: "iap" },
          scopes: ["session.read", "session.control", "session.files"]
        })
      )
    );

    await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toEqual({
      userId: "developer@ashler.ai",
      email: "developer@ashler.ai",
      projectScope: "ashler-staging",
      capabilities: ["session.read", "session.control", "session.files"],
      credential: "scaffold"
    });
  });

  it("fails closed on resource, actor schema, principal, capability, and upstream failures", async () => {
    const invalid = [
      { ok: true, resource: "https://other.example", actor: { sub: "dev@ashler.ai", auth: "iap" }, scopes: ["session.read", "session.control"] },
      { ok: true, resource: env.SCAFFOLD_CONTROL_PLANE_URL, user: { sub: "dev@ashler.ai" }, scopes: ["session.read", "session.control"] },
      { ok: true, resource: env.SCAFFOLD_CONTROL_PLANE_URL, actor: { sub: "", auth: "iap" }, scopes: ["session.read", "session.control"] },
      { ok: true, resource: env.SCAFFOLD_CONTROL_PLANE_URL, actor: { sub: "dev@ashler.ai", auth: "iap" }, scopes: ["session.read"] }
    ];
    for (const body of invalid) {
      vi.stubGlobal("fetch", vi.fn(async () => Response.json(body)));
      await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toBeUndefined();
    }
    vi.stubGlobal("fetch", vi.fn(async () => new Response("denied", { status: 401 })));
    await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toBeUndefined();
  });
});
