export interface Env {
  AUTH_GRANTS: DurableObjectNamespace;
  ENVIRONMENT: string;
  SCAFFOLD_CONTROL_PLANE_URL: string;
  SCAFFOLD_PROJECT_SCOPE: string;
  SCAFFOLD_DEPLOYMENT_ID: string;
  SCAFFOLD_REQUIRED_CAPABILITIES: string;
  SESSION_ROOMS: DurableObjectNamespace;
  DEVICE_ROOMS: DurableObjectNamespace;
  PREVIEW_ROOMS: DurableObjectNamespace;
  /** Per-user workspace registries (`reg1/{orgId}/{userId}`) — the row-table
   * replacement for the Loro workspace doc (docs/registry-sync.md). */
  REGISTRY_ROOMS: DurableObjectNamespace;
  /** chat2 session rooms (`chat2/{chatId}`) — dumb authenticated log relays
   * replacing SessionRoom's loro-aware s2 rooms (docs/chat2-sync.md). */
  CHAT_ROOMS: DurableObjectNamespace;
  BLOBS: R2Bucket;
  /** Isolated authenticated release artifacts; never an upstream or shared Crew bucket. */
  RELEASES: R2Bucket;
  /** Scaffold resource-bound tokens, or local-only development credentials. */
  AUTH_MODE: string;
  /** APNs auth key (contents of AuthKey_XXXX.p8, wrangler secret) and its
   * key id. Unset ⇒ session notifications are decided and logged, not sent. */
  APNS_KEY_P8?: string;
  APNS_KEY_ID?: string;
  /** Explicit Apple team and canary bundle id; no upstream credential fallback. */
  APNS_TEAM_ID?: string;
  APNS_TOPIC?: string;
}

/** APNs settings, when push is set up for this deployment. */
export const apnsConfig = (env: Env) =>
  env.APNS_KEY_P8 && env.APNS_KEY_ID && env.APNS_TEAM_ID && env.APNS_TOPIC
    ? {
        keyP8: env.APNS_KEY_P8,
        keyId: env.APNS_KEY_ID,
        teamId: env.APNS_TEAM_ID,
        topic: env.APNS_TOPIC
      }
    : undefined;

/** Header the Worker stamps on requests it forwards into DOs after verifying
 * the caller's JWT. DOs trust it blindly — they are only reachable through
 * the Worker (design §2: "DO never sees an unauthenticated frame"). */
export const AUTH_USER_HEADER = "x-zeron-auth-user";

/** Header the Worker stamps on requests forwarded into workspace-doc rooms
 * (`ws/{orgId}`). Membership (JWT org claim == orgId) is enforced at the
 * Worker; the SessionRoom DO sees this and skips its per-chat
 * claim-on-first-join ownership discipline for the room. */
export const ROOM_KIND_HEADER = "x-zeron-room-kind";
