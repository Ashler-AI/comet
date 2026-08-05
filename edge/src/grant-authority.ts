import { AuthGrant as StoredAuthGrant } from "./auth-routes";
import { GRANT_EVENT_HEADER, type Env } from "./env";

interface GrantAuthorityRecord {
  grantId: string;
  projectId: string;
  sessionId: string;
  accessExpiresAt?: number;
  revokedAt?: number;
}

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;

const hasGrantScope = (value: unknown): value is GrantAuthorityRecord => {
  if (!value || typeof value !== "object") return false;
  const record = value as Partial<GrantAuthorityRecord>;
  return (
    typeof record.grantId === "string" &&
    ID_RE.test(record.grantId) &&
    typeof record.projectId === "string" &&
    ID_RE.test(record.projectId) &&
    typeof record.sessionId === "string" &&
    ID_RE.test(record.sessionId)
  );
};

const isActiveGrant = (
  value: unknown,
  expectedGrantId: string | null,
  now: number
): value is GrantAuthorityRecord =>
  hasGrantScope(value) &&
  value.grantId === expectedGrantId &&
  Number.isSafeInteger(value.accessExpiresAt) &&
  (value.accessExpiresAt as number) > now &&
  value.revokedAt === undefined;

/**
 * Adds capability-state checks and revocation delivery without exposing a new
 * public route. The grant DO remains the source of truth; room attachments only
 * carry its identifier and the expiry verified when the socket connected.
 */
export class AuthGrant extends StoredAuthGrant {
  constructor(
    private readonly authorityCtx: DurableObjectState,
    private readonly authorityEnv: Env
  ) {
    super(authorityCtx, authorityEnv);
  }


  override async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/status") {
      if (request.headers.get(GRANT_EVENT_HEADER) !== "status") {
        return new Response(null, { status: 403 });
      }
      const record = await this.authorityCtx.storage.get<unknown>("grant");
      const active = isActiveGrant(record, url.searchParams.get("grantId"), Date.now());
      return new Response(null, { status: active ? 204 : 401 });
    }

    const isRevocation = request.method === "POST" && url.pathname === "/revoke";
    const stored = isRevocation
      ? await this.authorityCtx.storage.get<unknown>("grant")
      : undefined;
    const record = hasGrantScope(stored) ? stored : undefined;
    const response = await super.fetch(request);

    if (isRevocation && response.ok && record) {
      const room = this.authorityEnv.SESSION_ROOMS.get(
        this.authorityEnv.SESSION_ROOMS.idFromName(
          `s3/${record.projectId}/${record.sessionId}`
        )
      );
      try {
        const notification = await room.fetch(
          new Request("https://session.internal/grant-revoked", {
            method: "POST",
            headers: {
              [GRANT_EVENT_HEADER]: "revoke",
              "content-type": "application/json"
            },
            body: JSON.stringify({ grantId: record.grantId })
          })
        );
        if (!notification.ok) throw new Error(`room returned ${notification.status}`);
      } catch (error) {
        // Authority is already revoked durably. Rooms fail closed on their next
        // inbound or outbound frame if this best-effort immediate signal fails.
        console.error("grant revocation delivery failed", String(error));
      }
    }

    return response;
  }
}
