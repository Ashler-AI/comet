import { afterEach, describe, expect, it, vi } from "vitest";
import { fetchReleaseObject, type ReleaseFeedEnv } from "./release-feed";

const env: ReleaseFeedEnv = {
  COMET_RELEASES_GCS_BUCKET: "private-releases",
  GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "unused@example.invalid",
  GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "unused"
};

afterEach(() => vi.unstubAllGlobals());

describe("authenticated release feed", () => {
  it("obtains a fresh server credential for every manifest and artifact request", async () => {
    const authorizations: string[] = [];
    const redirects: Array<RequestInit["redirect"]> = [];
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_input: string | URL | Request, init?: RequestInit) => {
        authorizations.push(new Headers(init?.headers).get("authorization") ?? "");
        redirects.push(init?.redirect);
        return new Response("release bytes", {
          headers: { "content-type": "application/octet-stream", etag: '"digest"' }
        });
      })
    );
    let issued = 0;
    const accessToken = async () => `server-token-${++issued}`;

    const manifest = await fetchReleaseObject(env, "manifest.json", "GET", accessToken);
    const artifact = await fetchReleaseObject(
      env,
      "comet-0.2.0-linux-x86_64.tar.gz",
      "GET",
      accessToken
    );

    expect(authorizations).toEqual(["Bearer server-token-1", "Bearer server-token-2"]);
    expect(redirects).toEqual(["manual", "manual"]);
    expect(manifest.headers.get("cache-control")).toBe("private, no-store");
    expect(artifact.headers.get("cache-control")).toBe(
      "private, max-age=31536000, immutable"
    );
  });

  it("keeps each moving desktop and Scaffold channel uncached", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("release bytes", { status: 200 }))
    );

    for (const file of [
      "desktop-manifest.json",
      "desktop-SHA256SUMS",
      "desktop-latest.txt",
      "scaffold-manifest.json",
      "scaffold-SHA256SUMS",
      "scaffold-latest.txt"
    ]) {
      const response = await fetchReleaseObject(env, file, "GET", async () => "token");
      expect(response.headers.get("cache-control"), file).toBe("private, no-store");
    }
  });

  it("does not expose upstream authorization failures", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => Response.json({ secret: "upstream" }, { status: 403 })));

    const response = await fetchReleaseObject(env, "manifest.json", "GET", async () => "token");

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({ error: "release_fetch_failed" });
  });
});
