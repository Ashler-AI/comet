import { describe, expect, it } from "vitest";
import worker from "./index";
import type { Env } from "./env";

const ALL_CAPABILITIES =
  "session.read session.chat session.control session.annotate session.invite session.files session.environment";

type R2Value = ReadableStream | ArrayBuffer | ArrayBufferView | string | null | Blob;

interface StoredObject {
  readonly body: ArrayBuffer;
  readonly contentType: string;
}

const bodyBytes = async (value: R2Value): Promise<ArrayBuffer> => {
  if (value === null) return new ArrayBuffer(0);
  if (typeof value === "string") {
    const bytes = new TextEncoder().encode(value);
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
  }
  if (value instanceof ArrayBuffer) return value.slice(0);
  if (ArrayBuffer.isView(value)) {
    return value.buffer.slice(value.byteOffset, value.byteOffset + value.byteLength) as ArrayBuffer;
  }
  if (value instanceof Blob) return value.arrayBuffer();
  return new Response(value).arrayBuffer();
};

const storedR2Object = (key: string, stored: StoredObject): R2Object => ({
  key,
  version: "1",
  size: stored.body.byteLength,
  etag: key,
  httpEtag: `"${key}"`,
  checksums: { toJSON: () => ({}) },
  uploaded: new Date(0),
  httpMetadata: { contentType: stored.contentType },
  storageClass: "Standard",
  writeHttpMetadata(headers: Headers): void {
    headers.set("content-type", stored.contentType);
  }
});

const storedR2ObjectBody = (key: string, stored: StoredObject): R2ObjectBody => ({
  ...storedR2Object(key, stored),
  body: new Blob([stored.body]).stream(),
  bodyUsed: false,
  arrayBuffer: async () => stored.body.slice(0),
  bytes: async () => new Uint8Array(stored.body.slice(0)),
  text: async () => new TextDecoder().decode(stored.body),
  json: async <T>() => JSON.parse(new TextDecoder().decode(stored.body)) as T,
  blob: async () => new Blob([stored.body]),
  writeHttpMetadata(headers: Headers): void {
    headers.set("content-type", stored.contentType);
  }
});

class MemoryBucket {
  readonly objects = new Map<string, StoredObject>();

  get keys(): readonly string[] {
    return [...this.objects.keys()].sort();
  }

  async put(key: string, value: R2Value, options?: R2PutOptions): Promise<R2Object> {
    const httpMetadata = options?.httpMetadata;
    const contentType =
      httpMetadata instanceof Headers
        ? httpMetadata.get("content-type") ?? "application/octet-stream"
        : httpMetadata?.contentType ?? "application/octet-stream";
    const stored = { body: await bodyBytes(value), contentType };
    this.objects.set(key, stored);
    return storedR2Object(key, stored);
  }

  async get(key: string): Promise<R2ObjectBody | null> {
    const stored = this.objects.get(key);
    return stored ? storedR2ObjectBody(key, stored) : null;
  }

  async head(key: string): Promise<R2Object | null> {
    const stored = this.objects.get(key);
    return stored ? storedR2Object(key, stored) : null;
  }
}

const edgeEnv = (
  bucket: MemoryBucket,
  projectScope: Cloudflare.Env["SCAFFOLD_PROJECT_SCOPE"],
  capabilities = ALL_CAPABILITIES
): Env =>
  ({
    BLOBS: bucket as unknown as R2Bucket,
    AUTH_MODE: "dev",
    ENVIRONMENT: "local",
    SCAFFOLD_CONTROL_PLANE_URL: "http://127.0.0.1:8788",
    SCAFFOLD_PROJECT_SCOPE: projectScope,
    SCAFFOLD_REQUIRED_CAPABILITIES: capabilities,
    SESSION_ROOMS: {} as Env["SESSION_ROOMS"],
    DEVICE_ROOMS: {} as Env["DEVICE_ROOMS"],
    AUTH_GRANTS: {} as Env["AUTH_GRANTS"]
  }) as Env;

const request = (
  method: "PUT" | "GET" | "HEAD",
  hash: string,
  user: string,
  project: Cloudflare.Env["SCAFFOLD_PROJECT_SCOPE"],
  body?: string,
  contentType?: string
): Request => {
  const headers = new Headers({ authorization: `Bearer ${user}@${project}` });
  if (contentType) headers.set("content-type", contentType);
  return new Request(`http://127.0.0.1/attachments/${hash}`, { method, headers, body });
};

const hashOf = async (body: string): Promise<string> => {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(body));
  return [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
};

describe("project-scoped attachments", () => {
  it("allows another authorized user in the same project to read an upload", async () => {
    const bucket = new MemoryBucket();
    const env = edgeEnv(bucket, "ashler-local");
    const body = "shared attachment";
    const hash = await hashOf(body);

    const upload = await worker.fetch(
      request("PUT", hash, "alice", "ashler-local", body, "text/plain"),
      env
    );
    expect(upload.status).toBe(200);

    const read = await worker.fetch(request("GET", hash, "bob", "ashler-local"), env);
    expect(read.status).toBe(200);
    expect(read.headers.get("content-type")).toBe("text/plain");
    await expect(read.text()).resolves.toBe(body);
    expect(bucket.keys).toEqual([`att/ashler-local/${hash}`]);
  });

  it("isolates identical content hashes into distinct project objects", async () => {
    const bucket = new MemoryBucket();
    const local = edgeEnv(bucket, "ashler-local");
    const staging = edgeEnv(bucket, "ashler-staging");
    const body = "same immutable bytes";
    const hash = await hashOf(body);

    const localUpload = await worker.fetch(
      request("PUT", hash, "alice", "ashler-local", body, "application/x-local"),
      local
    );
    expect(localUpload.status).toBe(200);

    const crossProjectRead = await worker.fetch(
      request("GET", hash, "mallory", "ashler-staging"),
      staging
    );
    expect(crossProjectRead.status).toBe(404);

    const stagingUpload = await worker.fetch(
      request("PUT", hash, "mallory", "ashler-staging", body, "application/x-staging"),
      staging
    );
    expect(stagingUpload.status).toBe(200);
    expect(bucket.keys).toEqual([
      `att/ashler-local/${hash}`,
      `att/ashler-staging/${hash}`
    ]);

    const localRead = await worker.fetch(request("HEAD", hash, "bob", "ashler-local"), local);
    const stagingRead = await worker.fetch(
      request("HEAD", hash, "eve", "ashler-staging"),
      staging
    );
    expect(localRead.headers.get("content-type")).toBe("application/x-local");
    expect(stagingRead.headers.get("content-type")).toBe("application/x-staging");
  });

  it("keeps capability authorization and immutable hash validation fail-closed", async () => {
    const bucket = new MemoryBucket();
    const body = "immutable attachment";
    const hash = await hashOf(body);
    const withoutFiles = edgeEnv(bucket, "ashler-local", "session.read");

    const forbidden = await worker.fetch(
      request("PUT", hash, "alice", "ashler-local", body),
      withoutFiles
    );
    expect(forbidden.status).toBe(403);
    expect(bucket.keys).toEqual([]);

    const mismatch = await worker.fetch(
      request("PUT", "0".repeat(64), "alice", "ashler-local", body),
      edgeEnv(bucket, "ashler-local")
    );
    expect(mismatch.status).toBe(400);
    await expect(mismatch.json()).resolves.toEqual({ error: "hash_mismatch" });
    expect(bucket.keys).toEqual([]);
  });
});
