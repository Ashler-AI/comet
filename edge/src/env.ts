export type Env = Cloudflare.Env;

/** Trusted identity headers are replaced after authentication before DO forwarding. */
export const AUTH_USER_HEADER = "x-comet-auth-user";
export const AUTH_PROJECT_HEADER = "x-comet-auth-project";
export const AUTH_CAPABILITIES_HEADER = "x-comet-auth-capabilities";
export const AUTH_GRANT_HEADER = "x-comet-auth-grant";
export const ROOM_KIND_HEADER = "x-comet-room-kind";
export const GRANT_EVENT_HEADER = "x-comet-internal-grant-event";

export const stripTrustedAuthHeaders = (headers: Headers): void => {
  headers.delete(AUTH_USER_HEADER);
  headers.delete(AUTH_PROJECT_HEADER);
  headers.delete(AUTH_CAPABILITIES_HEADER);
  headers.delete(AUTH_GRANT_HEADER);
  headers.delete(GRANT_EVENT_HEADER);
  headers.delete(ROOM_KIND_HEADER);
};
