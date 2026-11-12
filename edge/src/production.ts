/**
 * Crew production native Edge: a separate Worker beside the untouched legacy
 * production Worker (`ashler-comet-edge-production`, comet.internal.ashler.com).
 *
 * Legacy and native issue the same credential shapes (`cs1.` device grants,
 * `sc_rc_` Scaffold tokens), so one Worker cannot admit both without consulting
 * two grant authorities. Production therefore keeps them apart by origin: legacy
 * sessions keep their exact deployed Worker, native clients use this one.
 *
 * The legacy Worker stays the only GCS release reader. Release reads are handed
 * to it over a service binding with the caller's own credential; it
 * authenticates them itself and only issues GCS GET/HEAD on that route.
 */
import native from "./index";
import { AuthGrant } from "./auth-routes";
import { DeviceRoom } from "./device-room";
import { PreviewRoom } from "./preview-room";
import { RegistryRoom } from "./registry-room";
import { ChatRoom } from "./chat-room";
import type { Env } from "./env";
import { nativeBlobStore } from "./staging-native";

const RELEASE_FILE_RE = /^[A-Za-z0-9._-]{1,200}$/;

/** Raw production bindings, exactly as configured in `env.production`. */
export interface ProductionEnv extends Omit<Env, "SESSION_ROOMS" | "NATIVE_GRANT_EPOCHS" | "BLOBS" | "APNS_KEY_P8"> {
  /** Shared with the legacy Worker; native access is fenced to `native-v2/`. */
  BLOBS: R2Bucket;
  /** The legacy production Worker. Used only for authenticated release reads. */
  LEGACY_EDGE: Fetcher;
  APNS_PRIVATE_KEY?: string;
}

/** Native code never reads SESSION_ROOMS; fail loudly instead of guessing a namespace. */
const retiredSessionRooms = new Proxy({}, {
  get() { throw new Error("SESSION_ROOMS is not bound in native production"); }
}) as DurableObjectNamespace;

/** The only env native HTTP handlers and Durable Objects see in production. */
export const activeProductionEnv = (raw: ProductionEnv): Env => {
  const { LEGACY_EDGE: _legacy, APNS_PRIVATE_KEY, BLOBS, ...rest } = raw;
  return {
    ...rest,
    SESSION_ROOMS: retiredSessionRooms,
    NATIVE_GRANT_EPOCHS: raw.AUTH_GRANTS,
    BLOBS: nativeBlobStore(BLOBS),
    APNS_KEY_P8: APNS_PRIVATE_KEY
  };
};

export class NativeAuthGrant extends AuthGrant {
  constructor(ctx: DurableObjectState, raw: ProductionEnv) { super(ctx, activeProductionEnv(raw)); }
}
export class NativeDeviceRoom extends DeviceRoom {
  constructor(ctx: DurableObjectState, raw: ProductionEnv) { super(ctx, activeProductionEnv(raw)); }
}
export class NativePreviewRoom extends PreviewRoom {
  constructor(ctx: DurableObjectState, raw: ProductionEnv) { super(ctx, activeProductionEnv(raw)); }
}
export class NativeRegistryRoom extends RegistryRoom {
  constructor(ctx: DurableObjectState, raw: ProductionEnv) { super(ctx, activeProductionEnv(raw)); }
}
export class NativeChatRoom extends ChatRoom {
  constructor(ctx: DurableObjectState, raw: ProductionEnv) { super(ctx, activeProductionEnv(raw)); }
}

export default {
  async fetch(request: Request, raw: ProductionEnv): Promise<Response> {
    const parts = new URL(request.url).pathname.split("/").filter(Boolean);
    if (parts[0] === "api" && parts[1] === "releases") {
      // Same single release route as staging; the legacy Worker owns it.
      if (parts.length === 3 && RELEASE_FILE_RE.test(parts[2]) &&
          (request.method === "GET" || request.method === "HEAD")) {
        return raw.LEGACY_EDGE.fetch(request);
      }
      return Response.json({ error: "not_found" }, { status: 404 });
    }
    if (parts[0] === "releases" || parts[0] === "install.sh") return Response.json({ error: "not_found" }, { status: 404 });
    return native.fetch(request, activeProductionEnv(raw));
  }
} satisfies ExportedHandler<ProductionEnv>;
