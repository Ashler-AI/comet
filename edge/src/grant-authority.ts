import { AuthGrant as StoredAuthGrant } from "./auth-routes";
import { GRANT_EVENT_HEADER, type Env } from "./env";

interface GrantAuthorityRecord {
  grantId: string;
  projectId: string;
  sessionId: string;
  accessExpiresAt?: number;
  revokedAt?: number;
}

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
      const record = await this.authorityCtx.storage.get<GrantAuthorityRecord>("grant");
      const active =
        record?.grantId === url.searchParams.get("grantId") &&
        !record.revokedAt &&
        typeof record.accessExpiresAt === "number" &&
        record.accessExpiresAt > Date.now();
      return new Response(null, { status: active ? 204 : 401 });
    }

    const isRevocation = request.method === "POST" && url.pathname === "/revoke";
    const record = isRevocation
      ? await this.authorityCtx.storage.get<GrantAuthorityRecord>("grant")
      : undefined;
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
