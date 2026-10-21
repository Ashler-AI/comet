// Types only: index.js and its companion WASM are the immutable deployed ae2 bundle.
export interface LegacyEnv {
  SESSION_ROOMS: DurableObjectNamespace;
  DEVICE_ROOMS: DurableObjectNamespace;
  AUTH_GRANTS: DurableObjectNamespace;
  BLOBS: Pick<R2Bucket, "get" | "head" | "put">;
  AUTH_MODE: string;
  ENVIRONMENT: string;
  SCAFFOLD_CONTROL_PLANE_URL: string;
  SCAFFOLD_PROJECT_SCOPE: string;
  SCAFFOLD_REQUIRED_CAPABILITIES: string;
  COMET_RELEASES_GCS_BUCKET?: string;
  GCP_RELEASE_SERVICE_ACCOUNT_EMAIL?: string;
  GCP_RELEASE_SERVICE_ACCOUNT_PRIVATE_KEY?: string;
}
export class AuthGrant implements DurableObject {
  constructor(ctx: DurableObjectState, env: LegacyEnv);
  fetch(request: Request): Promise<Response>;
}
export class DeviceRoom implements DurableObject {
  constructor(ctx: DurableObjectState, env: LegacyEnv);
  fetch(request: Request): Promise<Response>;
}
export class SessionRoom implements DurableObject {
  constructor(ctx: DurableObjectState, env: LegacyEnv);
  fetch(request: Request): Promise<Response>;
  alarm(): Promise<void>;
}
export function sessionRoomKey(
  identity: { projectScope: string; credential?: string; [key: string]: unknown },
  sessionId: string,
  requestedDeploymentId?: string | null
): string;
declare const legacy: {
  fetch(request: Request, env: LegacyEnv): Promise<Response>;
};
export default legacy;
