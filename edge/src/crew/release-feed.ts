export type ReleaseFeedEnv = {
  COMET_RELEASES_GCS_BUCKET?: string;
  GCP_RELEASE_SERVICE_ACCOUNT_EMAIL?: string;
  GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY?: string;
};

type AccessTokenSource = (env: ReleaseFeedEnv) => Promise<string>;

const encoder = new TextEncoder();
const TOKEN_ENDPOINT = "https://oauth2.googleapis.com/token";
const STORAGE_SCOPE = "https://www.googleapis.com/auth/devstorage.read_only";
const LATEST_ALIAS_RE = /^(?:(?:desktop(?:-staging)?|scaffold)-(?:manifest\.json|SHA256SUMS|latest\.txt)|manifest\.json|SHA256SUMS|install\.sh|latest\.txt)$/;

const base64Url = (bytes: Uint8Array): string => {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
};

const privateKeyBytes = (value: string): Uint8Array => {
  const base64 = value
    .trim()
    .replaceAll("\\n", "\n")
    .replace(/-----BEGIN PRIVATE KEY-----|-----END PRIVATE KEY-----|\s/g, "");
  if (!base64) throw new Error("release service account private key is unavailable");
  return Uint8Array.from(atob(base64), (character) => character.charCodeAt(0));
};

const googleAccessToken: AccessTokenSource = async (env) => {
  const email = env.GCP_RELEASE_SERVICE_ACCOUNT_EMAIL?.trim();
  const privateKey = env.GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY?.trim();
  if (!email || !privateKey) throw new Error("release service account is unavailable");

  const now = Math.floor(Date.now() / 1000);
  const header = base64Url(encoder.encode(JSON.stringify({ alg: "RS256", typ: "JWT" })));
  const claims = base64Url(
    encoder.encode(
      JSON.stringify({
        iss: email,
        scope: STORAGE_SCOPE,
        aud: TOKEN_ENDPOINT,
        iat: now,
        exp: now + 3600
      })
    )
  );
  const unsigned = `${header}.${claims}`;
  const key = await crypto.subtle.importKey(
    "pkcs8",
    privateKeyBytes(privateKey),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const signature = new Uint8Array(
    await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, encoder.encode(unsigned))
  );
  const response = await fetch(TOKEN_ENDPOINT, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer",
      assertion: `${unsigned}.${base64Url(signature)}`
    }),
    redirect: "manual"
  });
  if (!response.ok) throw new Error(`release token exchange failed (${response.status})`);
  const body = (await response.json()) as { access_token?: unknown };
  if (typeof body.access_token !== "string" || !body.access_token) {
    throw new Error("release token exchange returned no access token");
  }
  return body.access_token;
};

const errorResponse = (error: string, status: number, method: "GET" | "HEAD"): Response =>
  new Response(method === "HEAD" ? null : JSON.stringify({ error }), {
    status,
    headers: { "content-type": "application/json", "cache-control": "private, no-store" }
  });

// The router authenticates the caller; only the server's GCS credential reaches
// Google. Both legacy /api/releases and native /releases serve these same bytes.
export const fetchReleaseObject = async (
  env: ReleaseFeedEnv,
  file: string,
  method: "GET" | "HEAD",
  accessToken: AccessTokenSource = googleAccessToken
): Promise<Response> => {
  if (!/^[A-Za-z0-9._-]{1,200}$/.test(file) || file.includes("..")) {
    return errorResponse("bad_request", 400, method);
  }
  const bucket = env.COMET_RELEASES_GCS_BUCKET?.trim();
  if (!bucket) return errorResponse("release_feed_unavailable", 503, method);

  let token: string;
  try {
    token = await accessToken(env);
  } catch {
    return errorResponse("release_feed_unavailable", 503, method);
  }

  const object = encodeURIComponent(`releases/${file}`);
  let response: Response;
  try {
    response = await fetch(
      `https://storage.googleapis.com/download/storage/v1/b/${encodeURIComponent(bucket)}/o/${object}?alt=media`,
      { method, headers: { authorization: `Bearer ${token}` }, redirect: "manual" }
    );
  } catch {
    return errorResponse("release_fetch_failed", 502, method);
  }
  if (response.status === 404) return errorResponse("not_found", 404, method);
  if (!response.ok) return errorResponse("release_fetch_failed", 502, method);

  const headers = new Headers();
  for (const name of ["content-type", "content-length", "content-disposition", "etag", "last-modified"]) {
    const value = response.headers.get(name);
    if (value) headers.set(name, value);
  }
  headers.set(
    "cache-control",
    LATEST_ALIAS_RE.test(file) ? "private, no-store" : "private, max-age=31536000, immutable"
  );
  return new Response(method === "HEAD" ? null : response.body, { status: 200, headers });
};
