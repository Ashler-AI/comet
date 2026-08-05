import { readFileSync } from "node:fs";
import { fileURLToPath, URL as NodeUrl } from "node:url";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AuthGrant, handleAuthenticatedAuthRoute, parseGrantToken } from "./auth-routes";
import type { Verified } from "./auth";

describe("device grant tokens", () => {
  it("accepts only typed, bounded tokens", () => {
    const id = "a".repeat(32);
    const secret = "b".repeat(64);
    expect(parseGrantToken(`cg1.${id}.${secret}`, "cg1")).toEqual({ id, secret });
    expect(parseGrantToken(`cs1.${id}.${secret}`, "cg1")).toBeUndefined();
    expect(parseGrantToken(`cg1.${id}.short`, "cg1")).toBeUndefined();
    expect(parseGrantToken(`cg1.${id}.${secret}.extra`, "cg1")).toBeUndefined();
  });
});

describe("device access token rotation", () => {
  it("invalidates the previous token when refreshing", async () => {
    const records = new Map<string, unknown>();
    const grant = new AuthGrant({
      storage: {
        get: async (key: string) => records.get(key),
        put: async (key: string, value: unknown) => {
          records.set(key, value);
        }
      }
    } as unknown as DurableObjectState, {} as Env);
    const grantId = "a".repeat(32);
    const joinSecret = "b".repeat(64);
    const issuedAt = Date.now();
    const issue = await grant.fetch(
      new Request("https://grant.internal/issue", {
        method: "POST",
        body: JSON.stringify({
          grantId,
          ownerUserId: "developer@ashler.ai",
          email: "developer@ashler.ai",
          projectId: "ashler-staging",
          deploymentId: "ashler-staging",
          sandboxId: "sandbox-789",
          targetDeviceId: "comet-scaffold-sandbox-789",
          sessionId: "session-456",
          capabilities: ["session.read"],
          grantHash: await crypto.subtle
            .digest("SHA-256", new TextEncoder().encode(joinSecret))
            .then((digest) =>
              btoa(String.fromCharCode(...new Uint8Array(digest)))
                .replaceAll("+", "-")
                .replaceAll("/", "_")
                .replaceAll("=", "")
            ),
          issuedAt,
          grantExpiresAt: issuedAt + 60_000
        })
      })
    );
    expect(issue.status).toBe(200);
    const exchange = await grant.fetch(
      new Request("https://grant.internal/exchange", {
        method: "POST",
        body: JSON.stringify({ secret: joinSecret })
      })
    );
    const original = (await exchange.json()) as { accessToken: string };
    const originalParts = parseGrantToken(original.accessToken, "cs1")!;
    const refresh = await grant.fetch(
      new Request("https://grant.internal/refresh", {
        method: "POST",
        body: JSON.stringify({ secret: originalParts.secret })
      })
    );
    const rotated = (await refresh.json()) as {
      accessToken: string;
      projectId: string;
      deploymentId: string;
      sandboxId: string;
      targetDeviceId: string;
      sessionId: string;
    };
    expect(rotated.accessToken).not.toBe(original.accessToken);
    expect(rotated).toMatchObject({
      projectId: "ashler-staging",
      deploymentId: "ashler-staging",
      sandboxId: "sandbox-789",
      targetDeviceId: "comet-scaffold-sandbox-789",
      sessionId: "session-456"
    });
    expect(
      (
        await grant.fetch(
          new Request("https://grant.internal/authenticate", {
            method: "POST",
            body: JSON.stringify({ secret: originalParts.secret })
          })
        )
      ).status
    ).toBe(401);
  });
});

describe("device grant issuance", () => {
  afterEach(() => vi.restoreAllMocks());

  const identity: Verified = {
    userId: "developer@ashler.ai",
    email: "developer@ashler.ai",
    projectScope: "ashler-staging",
    capabilities: ["session.read", "session.control", "session.environment"],
    credential: "scaffold"
  };

  const request = (
    overrides: Partial<{
      deploymentId: string;
      sandboxId: string;
      targetDeviceId: string;
      sessionId: string;
    }> = {}
  ) =>
    new Request("https://comet.example/auth/device-grants", {
      method: "POST",
      headers: {
        authorization: "Bearer owner-session",
        "content-type": "application/json"
      },
      body: JSON.stringify({
        deploymentId: identity.projectScope,
        sandboxId: "sandbox-789",
        targetDeviceId: "comet-scaffold-sandbox-789",
        sessionId: "session-456",
        capabilities: ["session.read", "session.control"],
        ...overrides
      })
    });

  const envAndRecords = () => {
    const records: unknown[] = [];
    const env = {
      AUTH_MODE: "scaffold",
      ENVIRONMENT: "staging",
      SCAFFOLD_CONTROL_PLANE_URL: "https://scaffold.example",
      SCAFFOLD_PROJECT_SCOPE: identity.projectScope,
      SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.control session.environment",
      AUTH_GRANTS: {
        idFromName: (id: string) => id,
        get: () => ({
          fetch: async (_url: string, init: RequestInit) => {
            records.push(JSON.parse(String(init.body)));
            return new Response(JSON.stringify({ ok: true }));
          }
        })
      }
    } as unknown as Env;
    return { env, records };
  };

  it("derives every grant target field from Scaffold's authenticated runtime profile", async () => {
    const verifyTarget = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      Response.json({
        ok: true,
        profile: {
          version: "scaffold.comet-runtime.v1",
          projectId: identity.projectScope,
          deploymentId: identity.projectScope,
          sandboxId: "sandbox-789",
          targetDeviceId: "comet-scaffold-sandbox-789",
          sessionId: "session-456",
          actor: { sub: identity.userId }
        }
      })
    );
    const { env, records } = envAndRecords();
    const response = await handleAuthenticatedAuthRoute(
      request(),
      env,
      new URL("https://comet.example/auth/device-grants"),
      identity
    );
    expect(response?.status).toBe(200);
    expect(verifyTarget).toHaveBeenCalledWith(
      "https://scaffold.example/api/code-sandboxes/sandbox-789/comet-target/verify",
      expect.objectContaining({
        method: "POST",
        redirect: "manual",
        body: JSON.stringify({
          projectId: identity.projectScope,
          deploymentId: identity.projectScope,
          sandboxId: "sandbox-789",
          targetDeviceId: "comet-scaffold-sandbox-789",
          sessionId: "session-456"
        })
      })
    );
    expect(records).toHaveLength(1);
    expect(records[0]).toMatchObject({
      projectId: identity.projectScope,
      deploymentId: identity.projectScope,
      sandboxId: "sandbox-789",
      targetDeviceId: "comet-scaffold-sandbox-789",
      sessionId: "session-456"
    });
  });

  it("rejects a caller-selected stale or foreign sandbox/session tuple", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      Response.json({
        ok: true,
        profile: {
          version: "scaffold.comet-runtime.v1",
          projectId: identity.projectScope,
          deploymentId: identity.projectScope,
          sandboxId: "sandbox-789",
          targetDeviceId: "comet-scaffold-sandbox-789",
          sessionId: "session-456",
          actor: { sub: identity.userId }
        }
      })
    );
    const { env, records } = envAndRecords();
    const response = await handleAuthenticatedAuthRoute(
      request({
        sandboxId: "foreign-sandbox",
        targetDeviceId: "comet-scaffold-foreign-sandbox",
        sessionId: "stale-session"
      }),
      env,
      new URL("https://comet.example/auth/device-grants"),
      identity
    );
    expect(response?.status).toBe(403);
    expect(records).toHaveLength(0);
  });

  it.each([
    [
      "an unknown Scaffold target",
      404,
      { error: "remote_code_sandbox_not_found" }
    ],
    [
      "a target proof with the wrong actor schema",
      200,
      {
        ok: true,
        profile: {
          version: "scaffold.comet-runtime.v1",
          projectId: identity.projectScope,
          deploymentId: identity.projectScope,
          sandboxId: "sandbox-789",
          targetDeviceId: "comet-scaffold-sandbox-789",
          sessionId: "session-456",
          user: { sub: identity.userId }
        }
      }
    ]
  ] as const)("denies %s before mutating grant state", async (_name, status, proof) => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(Response.json(proof, { status }));
    const { env, records } = envAndRecords();
    const response = await handleAuthenticatedAuthRoute(
      request(),
      env,
      new URL("https://comet.example/auth/device-grants"),
      identity
    );
    expect(response?.status).toBe(403);
    expect(records).toHaveLength(0);
  });
});

describe("Wrangler environments", () => {
  it("keeps root inert and staging/production resources distinct", () => {
    const source = readFileSync(fileURLToPath(new NodeUrl("../wrangler.jsonc", import.meta.url)), "utf8");
    const withoutCommentLines = source
      .split("\n")
      .filter((line) => !line.trimStart().startsWith("//"))
      .join("\n");
    const config = JSON.parse(withoutCommentLines) as {
      account_id?: string;
      routes?: unknown;
      workers_dev: boolean;
      name: string;
      compatibility_date: string;
      env: Record<string, {
        name: string;
        workers_dev: boolean;
        preview_urls: boolean;
        routes: Array<{ pattern: string; custom_domain: boolean }>;
        r2_buckets: Array<{ bucket_name: string }>;
        vars: Record<string, string>;
      }>;
    };
    expect(config.account_id).toBeUndefined();
    expect(config.routes).toBeUndefined();
    expect(config.workers_dev).toBe(false);
    expect(config.name).toContain("local-only");
    expect(config.compatibility_date).toBe("2026-08-04");
    expect(config.env.staging.name).not.toBe(config.env.production.name);
    expect(config.env.staging.workers_dev).toBe(false);
    expect(config.env.production.workers_dev).toBe(false);
    expect(config.env.staging.preview_urls).toBe(false);
    expect(config.env.production.preview_urls).toBe(false);
    expect(config.env.staging.routes).toEqual([
      { pattern: "comet-staging.internal.ashler.com", custom_domain: true }
    ]);
    expect(config.env.production.routes).toEqual([
      { pattern: "comet.internal.ashler.com", custom_domain: true }
    ]);
    expect(config.env.staging.r2_buckets.map((bucket) => bucket.bucket_name)).not.toEqual(
      config.env.production.r2_buckets.map((bucket) => bucket.bucket_name)
    );
    expect(config.env.staging.vars.SCAFFOLD_CONTROL_PLANE_URL).not.toBe(
      config.env.production.vars.SCAFFOLD_CONTROL_PLANE_URL
    );
    expect(config.env.staging.vars.AUTH_MODE).toBe("scaffold");
    expect(config.env.production.vars.AUTH_MODE).toBe("scaffold");
  });
});
