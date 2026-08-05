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
  SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.chat session.control session.annotate session.invite session.files session.environment"
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


describe("development authentication", () => {
  it("uses configured internal capabilities without remote-code translation", async () => {
    const devEnv = {
      ...env,
      AUTH_MODE: "dev",
      ENVIRONMENT: "local",
      SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.files"
    } satisfies ScaffoldAuthEnv;

    await expect(
      authenticateScaffold(devEnv, new Request("http://localhost:8787/session/session-1/ws?token=local-developer"))
    ).resolves.toEqual({
      userId: "local-developer",
      email: "local-developer@dev.local",
      projectScope: "ashler-staging",
      capabilities: ["session.read", "session.files"],
      credential: "dev"
    });
  });
});

describe("Scaffold bearer validation", () => {
  it("maps all Platform remote-code scopes to internal session capabilities", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          ok: true,
          resource: "https://scaffold-staging.internal.ashler.com",
          actor: { sub: "Developer@Ashler.ai", auth: "iap" },
          scopes: [
            "remote_code:create",
            "remote_code:read",
            "remote_code:write",
            "remote_code:exec",
            "remote_code:lifecycle",
            "constructor"
          ]
        })
      )
    );

    await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toEqual({
      userId: "developer@ashler.ai",
      email: "developer@ashler.ai",
      projectScope: "ashler-staging",
      capabilities: [
        "session.invite",
        "session.read",
        "session.chat",
        "session.annotate",
        "session.files",
        "session.control",
        "session.environment"
      ],
      credential: "scaffold"
    });
  });

  it("keeps a read-only bearer least-privileged and rejects it when full capabilities are required", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          ok: true,
          resource: env.SCAFFOLD_CONTROL_PLANE_URL,
          actor: { sub: "reader@ashler.ai", auth: "iap" },
          scopes: ["remote_code:read"]
        })
      )
    );
    const readOnlyEnv = { ...env, SCAFFOLD_REQUIRED_CAPABILITIES: "session.read" };

    await expect(verifyScaffoldToken(readOnlyEnv, "sc_rc_reader")).resolves.toEqual({
      userId: "reader@ashler.ai",
      email: "reader@ashler.ai",
      projectScope: "ashler-staging",
      capabilities: ["session.read"],
      credential: "scaffold"
    });
    await expect(verifyScaffoldToken(env, "sc_rc_reader")).resolves.toBeUndefined();
  });

  it("fails closed on resource, actor schema, principal, capability, and upstream failures", async () => {
    const invalid = [
      {
        ok: true,
        resource: "https://other.example",
        actor: { sub: "dev@ashler.ai", auth: "iap" },
        scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"]
      },
      {
        ok: true,
        resource: env.SCAFFOLD_CONTROL_PLANE_URL,
        user: { sub: "dev@ashler.ai" },
        scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"]
      },
      {
        ok: true,
        resource: env.SCAFFOLD_CONTROL_PLANE_URL,
        actor: { sub: "", auth: "iap" },
        scopes: ["remote_code:create", "remote_code:read", "remote_code:write", "remote_code:exec", "remote_code:lifecycle"]
      },
      {
        ok: true,
        resource: env.SCAFFOLD_CONTROL_PLANE_URL,
        actor: { sub: "dev@ashler.ai", auth: "iap" },
        scopes: ["remote_code:read"]
      }
    ];
    for (const body of invalid) {
      vi.stubGlobal("fetch", vi.fn(async () => Response.json(body)));
      await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toBeUndefined();
    }
    vi.stubGlobal("fetch", vi.fn(async () => new Response("denied", { status: 401 })));
    await expect(verifyScaffoldToken(env, "sc_rc_test")).resolves.toBeUndefined();
  });
});
