import { describe, expect, it } from "vitest";
import worker from "./index";
import { DEBUG_PAGE_CSP } from "./debug-page";
import type { Env } from "./env";

const env = (): Env =>
  ({
    AUTH_MODE: "dev",
    ENVIRONMENT: "local",
    SCAFFOLD_CONTROL_PLANE_URL: "http://127.0.0.1:8788",
    SCAFFOLD_PROJECT_SCOPE: "ashler-local",
    SCAFFOLD_REQUIRED_CAPABILITIES:
      "session.read session.chat session.control session.annotate session.invite session.files session.environment",
    BLOBS: {} as Env["BLOBS"],
    SESSION_ROOMS: {} as Env["SESSION_ROOMS"],
    DEVICE_ROOMS: {} as Env["DEVICE_ROOMS"],
    AUTH_GRANTS: {} as Env["AUTH_GRANTS"]
  }) as Env;

describe("debug inspector shell", () => {
  it("serves the static shell without authentication", async () => {
    const response = await worker.fetch(new Request("http://127.0.0.1/debug"), env());
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/html");
    expect(response.headers.get("content-security-policy")).toBe(DEBUG_PAGE_CSP);
    expect(response.headers.get("x-robots-tag")).toContain("noindex");
    const body = await response.text();
    expect(body).toContain("comet session inspector");
    // The page speaks the DO's exact protocol versions — a drifted pin is a bug.
    expect(body).toContain("loro-crdt@1.13.7");
    expect(body).toContain("loro-protocol@0.3.0");
  });

  it("only the shell is public: non-GET /debug still authenticates", async () => {
    const response = await worker.fetch(
      new Request("http://127.0.0.1/debug", { method: "POST" }),
      env()
    );
    expect(response.status).toBe(401);
  });
});

describe("/whoami", () => {
  it("rejects unauthenticated requests", async () => {
    const response = await worker.fetch(new Request("http://127.0.0.1/whoami"), env());
    expect(response.status).toBe(401);
  });

  it("returns the verified identity for a dev bearer", async () => {
    const response = await worker.fetch(
      new Request("http://127.0.0.1/whoami", {
        headers: { authorization: "Bearer alice@ashler-local" }
      }),
      env()
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      userId: "alice",
      email: "alice@dev.local",
      projectScope: "ashler-local",
      capabilities: [
        "session.read",
        "session.chat",
        "session.control",
        "session.annotate",
        "session.invite",
        "session.files",
        "session.environment"
      ],
      credential: "dev",
      environment: "local"
    });
  });

  it("rejects a bearer scoped to another project", async () => {
    const response = await worker.fetch(
      new Request("http://127.0.0.1/whoami", {
        headers: { authorization: "Bearer alice@proj-b" }
      }),
      env()
    );
    expect(response.status).toBe(401);
  });
});
