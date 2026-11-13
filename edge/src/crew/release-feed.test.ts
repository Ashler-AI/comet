import { afterEach, describe, expect, it, vi } from "vitest";
import { exportPKCS8, generateKeyPair } from "jose";
import { fetchReleaseObject, type ReleaseFeedEnv } from "./release-feed";

const env: ReleaseFeedEnv = {
  COMET_RELEASES_GCS_BUCKET: "private-releases",
  GCP_RELEASE_SERVICE_ACCOUNT_EMAIL: "release-reader@example.invalid",
  GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: "unused"
};

afterEach(() => vi.unstubAllGlobals());

describe("authenticated private GCS release feed", () => {
  it("proxies the legacy staging manifest, archive and signature with fresh server credentials", async () => {
    const files = [
      "desktop-staging-manifest.json",
      "comet-staging-0.2.114-macos-arm64-app.tar.gz",
      "comet-staging-0.2.114-macos-arm64-app.tar.gz.sig"
    ];
    const manifest = JSON.stringify({
      version: "0.2.114",
      files: { [files[1]]: { sha256: "a".repeat(64) } }
    });
    const bodies = [manifest, "archive bytes", "signature bytes"];
    let fetched = 0;
    const upstream = vi.fn(async () => new Response(bodies[fetched++], {
      headers: { "content-type": "application/octet-stream", etag: '"digest"' }
    }));
    vi.stubGlobal("fetch", upstream);
    let issued = 0;
    const accessToken = vi.fn(async () => `server-token-${++issued}`);

    for (const [index, file] of files.entries()) {
      const response = await fetchReleaseObject(env, file, "GET", accessToken);
      expect(await response.text()).toBe(bodies[index]);
      expect(response.headers.get("etag")).toBe('"digest"');
      expect(response.headers.get("cache-control")).toBe(index === 0
        ? "private, no-store"
        : "private, max-age=31536000, immutable");
      expect(upstream).toHaveBeenNthCalledWith(index + 1,
        `https://storage.googleapis.com/download/storage/v1/b/private-releases/o/releases%2F${file}?alt=media`,
        { method: "GET", headers: { authorization: `Bearer server-token-${index + 1}` }, redirect: "manual" }
      );
    }
    expect(accessToken).toHaveBeenCalledTimes(3);
  });

  it.each([
    "manifest.json", "SHA256SUMS", "latest.txt", "install.sh",
    "desktop-manifest.json", "desktop-SHA256SUMS", "desktop-latest.txt",
    "desktop-staging-manifest.json", "desktop-staging-SHA256SUMS", "desktop-staging-latest.txt",
    "scaffold-manifest.json", "scaffold-SHA256SUMS", "scaffold-latest.txt"
  ])("never caches the moving %s alias", async (file) => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("release bytes")));
    const response = await fetchReleaseObject(env, file, "GET", async () => "token");
    expect(response.headers.get("cache-control")).toBe("private, no-store");
  });

  it("preserves HEAD metadata without returning a body or upstream private headers", async () => {
    const upstream = vi.fn(async () => new Response(null, { headers: {
      "content-type": "application/gzip", "content-length": "123",
      "content-disposition": 'attachment; filename="comet.tar.gz"',
      etag: '"digest"', "last-modified": "Tue, 20 Oct 2026 00:00:00 GMT",
      "set-cookie": "upstream-private=value", "x-goog-meta-private": "private"
    } }));
    vi.stubGlobal("fetch", upstream);
    const response = await fetchReleaseObject(env, "comet-0.2.114-linux-x86_64.tar.gz", "HEAD", async () => "token");
    expect(upstream).toHaveBeenCalledWith(expect.any(String), expect.objectContaining({ method: "HEAD" }));
    expect(await response.text()).toBe("");
    expect(response.headers.get("content-length")).toBe("123");
    expect(response.headers.get("content-disposition")).toBe('attachment; filename="comet.tar.gz"');
    expect(response.headers.get("last-modified")).toBe("Tue, 20 Oct 2026 00:00:00 GMT");
    expect(response.headers.get("set-cookie")).toBeNull();
    expect(response.headers.get("x-goog-meta-private")).toBeNull();
  });

  it.each([302, 401, 403, 404, 500])("fails closed on upstream %s without redirect or fallback", async (status) => {
    const upstream = vi.fn(async () => new Response("private upstream details", {
      status, headers: { location: "https://public.invalid/release" }
    }));
    vi.stubGlobal("fetch", upstream);
    const response = await fetchReleaseObject(env, "desktop-staging-manifest.json", "GET", async () => "token");
    expect(response.status).toBe(status === 404 ? 404 : 502);
    expect(await response.json()).toEqual({ error: status === 404 ? "not_found" : "release_fetch_failed" });
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    expect(response.headers.get("location")).toBeNull();
    expect(upstream).toHaveBeenCalledTimes(1);
  });

  it("fails closed when bindings, credentials or storage are unavailable", async () => {
    const upstream = vi.fn(async () => { throw new Error("private network detail"); });
    vi.stubGlobal("fetch", upstream);
    expect((await fetchReleaseObject({}, "manifest.json", "GET")).status).toBe(503);
    expect((await fetchReleaseObject({ COMET_RELEASES_GCS_BUCKET: "private-releases" }, "manifest.json", "GET")).status).toBe(503);
    expect(upstream).not.toHaveBeenCalled();
    const failedToken = await fetchReleaseObject(env, "manifest.json", "GET", async () => { throw new Error("private credential detail"); });
    expect(await failedToken.json()).toEqual({ error: "release_feed_unavailable" });
    expect(upstream).not.toHaveBeenCalled();
    const failedStorage = await fetchReleaseObject(env, "manifest.json", "HEAD", async () => "token");
    expect(failedStorage.status).toBe(502);
    expect(await failedStorage.text()).toBe("");
    expect(upstream).toHaveBeenCalledTimes(1);
  });

  it.each(["", "../secret", "nested/file", "..", "%2e%2e", "file?alt=json", "a".repeat(201)])("rejects invalid object name %s before obtaining credentials", async (file) => {
    const upstream = vi.fn();
    const accessToken = vi.fn(async () => "token");
    vi.stubGlobal("fetch", upstream);
    expect((await fetchReleaseObject(env, file, "GET", accessToken)).status).toBe(400);
    expect(accessToken).not.toHaveBeenCalled();
    expect(upstream).not.toHaveBeenCalled();
  });

  it("exchanges a signed read-only service-account assertion using the legacy escaped PEM binding", async () => {
    const key = await generateKeyPair("RS256", { extractable: true, modulusLength: 2048 });
    const pem = (await exportPKCS8(key.privateKey)).replaceAll("\n", "\\n");
    let assertion = "";
    const upstream = vi.fn(async (input: string, init?: RequestInit) => {
      if (input === "https://oauth2.googleapis.com/token") {
        expect(init?.method).toBe("POST");
        expect(init?.redirect).toBe("manual");
        const body = new URLSearchParams(init?.body as URLSearchParams);
        expect(body.get("grant_type")).toBe("urn:ietf:params:oauth:grant-type:jwt-bearer");
        assertion = body.get("assertion") ?? "";
        return Response.json({ access_token: "server-only-token" });
      }
      expect(new Headers(init?.headers).get("authorization")).toBe("Bearer server-only-token");
      return new Response("manifest bytes");
    });
    vi.stubGlobal("fetch", upstream);
    const response = await fetchReleaseObject({ ...env, GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY: pem }, "desktop-staging-manifest.json", "GET");
    expect(response.status).toBe(200);
    const [header, claims, signature] = assertion.split(".");
    const decode = (part: string) => Uint8Array.from(atob(part.replaceAll("-", "+").replaceAll("_", "/")), c => c.charCodeAt(0));
    expect(JSON.parse(new TextDecoder().decode(decode(header)))).toEqual({ alg: "RS256", typ: "JWT" });
    const payload = JSON.parse(new TextDecoder().decode(decode(claims)));
    expect(payload).toMatchObject({
      iss: env.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL,
      aud: "https://oauth2.googleapis.com/token",
      scope: "https://www.googleapis.com/auth/devstorage.read_only"
    });
    expect(payload.exp - payload.iat).toBe(3600);
    expect(await crypto.subtle.verify("RSASSA-PKCS1-v1_5", key.publicKey,
      decode(signature), new TextEncoder().encode(`${header}.${claims}`))).toBe(true);
    expect(upstream).toHaveBeenCalledTimes(2);
  });
});
