import { AuthGrant } from "./auth-routes";
import { DeviceRoom } from "./device-room";
import { PreviewRoom } from "./preview-room";
import { RegistryRoom } from "./registry-room";
import { ChatRoom } from "./chat-room";
import type { Env } from "./env";
import type { LegacyEnv } from "../legacy/ae2/index.js";

/** Raw canonical bindings. Never pass an active view to retained legacy code. */
export interface StagingEnv extends LegacyEnv, Omit<Env,
  "PREVIEW_ROOMS" | "REGISTRY_ROOMS" | "CHAT_ROOMS" | "SCAFFOLD_DEPLOYMENT_ID"
> {
  NATIVE_AUTH_GRANTS: DurableObjectNamespace;
  NATIVE_DEVICE_ROOMS: DurableObjectNamespace;
  NATIVE_PREVIEW_ROOMS: DurableObjectNamespace;
  NATIVE_REGISTRY_ROOMS: DurableObjectNamespace;
  NATIVE_CHAT_ROOMS: DurableObjectNamespace;
  APNS_PRIVATE_KEY?: string;
}

/** Legacy writes start att/ or backup/rooms/. This prefix cannot intersect either. */
export const NATIVE_BLOB_PREFIX = "native-v2/";

/** Every native storage operation is fenced; no raw bucket/list/delete escapes. */
export const nativeBlobStore = (bucket: Env["BLOBS"]): Env["BLOBS"] => ({
  get: (key: string, options?: R2GetOptions) => bucket.get(`${NATIVE_BLOB_PREFIX}${key}`, options),
  head: (key: string) => bucket.head(`${NATIVE_BLOB_PREFIX}${key}`),
  put: (key: string, value: Parameters<R2Bucket["put"]>[1], options?: R2PutOptions) =>
    bucket.put(`${NATIVE_BLOB_PREFIX}${key}`, value, options)
});

/** Used only by native HTTP and native constructors, including alarm wakes. */
export const activeNativeEnv = (raw: StagingEnv): Env => ({
  ...raw,
  AUTH_GRANTS: raw.NATIVE_AUTH_GRANTS,
  NATIVE_GRANT_EPOCHS: raw.NATIVE_AUTH_GRANTS,
  DEVICE_ROOMS: raw.NATIVE_DEVICE_ROOMS,
  PREVIEW_ROOMS: raw.NATIVE_PREVIEW_ROOMS,
  REGISTRY_ROOMS: raw.NATIVE_REGISTRY_ROOMS,
  CHAT_ROOMS: raw.NATIVE_CHAT_ROOMS,
  BLOBS: nativeBlobStore(raw.BLOBS),
  SCAFFOLD_PROJECT_SCOPE: "ashler-staging",
  SCAFFOLD_DEPLOYMENT_ID: "ashler-staging",
  SCAFFOLD_COMET_RUNTIME_VERSION: "scaffold.comet-runtime.v2",
  SCAFFOLD_REQUIRED_CAPABILITIES: "session.read session.sync.write session.chat session.control session.annotate session.files session.environment",
  APNS_KEY_P8: raw.APNS_PRIVATE_KEY
});

export class NativeAuthGrant extends AuthGrant {
  constructor(ctx: DurableObjectState, raw: StagingEnv) { super(ctx, activeNativeEnv(raw)); }
}
export class NativeDeviceRoom extends DeviceRoom {
  constructor(ctx: DurableObjectState, raw: StagingEnv) { super(ctx, activeNativeEnv(raw)); }
}
export class NativePreviewRoom extends PreviewRoom {
  constructor(ctx: DurableObjectState, raw: StagingEnv) { super(ctx, activeNativeEnv(raw)); }
}
export class NativeRegistryRoom extends RegistryRoom {
  constructor(ctx: DurableObjectState, raw: StagingEnv) { super(ctx, activeNativeEnv(raw)); }
}
export class NativeChatRoom extends ChatRoom {
  constructor(ctx: DurableObjectState, raw: StagingEnv) { super(ctx, activeNativeEnv(raw)); }
}
