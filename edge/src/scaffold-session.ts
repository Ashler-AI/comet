/**
 * Bounded reuse of one successful Scaffold session verification.
 *
 * Scaffold stays the only authority; this bounds how long its last positive
 * `/api/code-sandboxes/auth/session` answer may be reused for the same token:
 *   - fresh (verifiedAt + freshMs): reused without contacting Scaffold.
 *   - stale (until verifiedAt + graceMs): reused ONLY while Scaffold cannot
 *     answer (transport failure, timeout, 408/429/5xx, admission limits).
 *   - an explicit 401/403, or a definitive denial (a 2xx answer that fails the
 *     resource, actor or required-capability checks), purges it at once.
 * Reuse never extends itself: grace is anchored to the last successful
 * verification, never to the last request served from it.
 *
 * Keys are coordinator names (`scaffoldVerificationName`): SHA-256 over the
 * token plus authority URL, project, required capabilities, environment and
 * deployment. Nothing is shared across tokens, tenants or configurations.
 */
import type { Verified } from "./auth";

export const DEFAULT_SESSION_FRESH_MS = 60_000;
export const DEFAULT_SESSION_GRACE_MS = 15 * 60_000;
const MIN_SESSION_FRESH_MS = 100;
const MAX_SESSION_FRESH_MS = 5 * 60_000;
const MAX_SESSION_GRACE_MS = 60 * 60_000;
/** Coordinator and isolate clocks differ slightly; bounds are re-anchored to local time anyway. */
const MAX_CLOCK_SKEW_MS = 30_000;

/** One isolate's reusable sessions; each entry is well under 1 KiB. */
export const ISOLATE_SESSION_LIMIT = 256;
/** A stale isolate entry consults its coordinator for at most this long. */
export const ISOLATE_STALE_CHECK_MS = 2_000;
/** After the authority could not answer, reuse a stale entry this long before asking again. */
export const ISOLATE_RECHECK_MS = 5_000;

export interface SessionPolicy {
  readonly freshMs: number;
  readonly graceMs: number;
}

export interface SessionPolicyEnv {
  readonly SCAFFOLD_SESSION_FRESH_MS?: string;
  readonly SCAFFOLD_SESSION_GRACE_MS?: string;
}

const milliseconds = (value: string | undefined, fallback: number): number =>
  value !== undefined && /^\d{1,9}$/.test(value.trim()) ? Number(value.trim()) : fallback;

/** Operator bounds are clamped; an unset or malformed value keeps the default. */
export const sessionPolicy = (env: SessionPolicyEnv): SessionPolicy => {
  const freshMs = Math.min(MAX_SESSION_FRESH_MS,
    Math.max(MIN_SESSION_FRESH_MS, milliseconds(env.SCAFFOLD_SESSION_FRESH_MS, DEFAULT_SESSION_FRESH_MS)));
  const graceMs = Math.min(MAX_SESSION_GRACE_MS,
    Math.max(freshMs, milliseconds(env.SCAFFOLD_SESSION_GRACE_MS, DEFAULT_SESSION_GRACE_MS)));
  return { freshMs, graceMs };
};

export interface VerifiedSession {
  readonly identity: Verified;
  /** When Scaffold last answered positively for this exact token and configuration. */
  readonly verifiedAt: number;
  readonly freshUntil: number;
  readonly staleUntil: number;
}

export const verifiedSession = (identity: Verified, now: number, policy: SessionPolicy): VerifiedSession => ({
  identity, verifiedAt: now, freshUntil: now + policy.freshMs, staleUntil: now + policy.graceMs
});

/** Accepts only a well-formed record and can only shorten it to the current policy. */
export const boundedSession = (value: unknown, policy: SessionPolicy, now: number): VerifiedSession | undefined => {
  if (!value || typeof value !== "object") return undefined;
  const session = value as Partial<VerifiedSession>;
  const identity = session.identity;
  const { verifiedAt, freshUntil, staleUntil } = session;
  if (
    !identity || typeof identity !== "object" || identity.credential !== "scaffold" ||
    typeof identity.userId !== "string" || identity.userId.length === 0 ||
    !Array.isArray(identity.capabilities) || !identity.capabilities.every(value => typeof value === "string") ||
    typeof verifiedAt !== "number" || !Number.isSafeInteger(verifiedAt) ||
    typeof freshUntil !== "number" || !Number.isSafeInteger(freshUntil) ||
    typeof staleUntil !== "number" || !Number.isSafeInteger(staleUntil) ||
    verifiedAt > now + MAX_CLOCK_SKEW_MS || freshUntil < verifiedAt || staleUntil < freshUntil
  ) return undefined;
  const boundedStale = Math.min(staleUntil, verifiedAt + policy.graceMs, now + policy.graceMs);
  return {
    identity, verifiedAt,
    freshUntil: Math.min(freshUntil, verifiedAt + policy.freshMs, now + policy.freshMs, boundedStale),
    staleUntil: boundedStale
  };
};

export interface SessionEntry<Check> {
  session: VerifiedSession;
  /** Reuse a stale entry without consulting the coordinator until then. */
  recheckAt: number;
  /** The single in-flight coordinator consultation for this isolate and key. */
  check?: Promise<Check>;
}

/** Insertion-ordered LRU. Entries past their grace bound are dropped on access. */
export class SessionCache<Check> {
  private readonly entries = new Map<string, SessionEntry<Check>>();
  constructor(private readonly limit = ISOLATE_SESSION_LIMIT) {}

  get size(): number { return this.entries.size; }

  get(key: string, now: number): SessionEntry<Check> | undefined {
    const entry = this.entries.get(key);
    if (!entry) return undefined;
    if (now >= entry.session.staleUntil) {
      this.entries.delete(key);
      return undefined;
    }
    this.entries.delete(key);
    this.entries.set(key, entry);
    return entry;
  }

  /** A stale record (the coordinator's own fallback) is not re-asked before the recheck delay. */
  put(key: string, session: VerifiedSession, now: number, policy: SessionPolicy): void {
    const bounded = boundedSession(session, policy, now);
    this.entries.delete(key);
    if (!bounded || bounded.staleUntil <= now) return;
    this.entries.set(key, { session: bounded, recheckAt: bounded.freshUntil > now ? 0 : now + ISOLATE_RECHECK_MS });
    while (this.entries.size > this.limit) {
      const oldest = this.entries.keys().next();
      if (oldest.done) break;
      this.entries.delete(oldest.value);
    }
  }

  /** The authority could not answer: keep reusing within grace, but stop asking for a while. */
  deferRecheck(key: string, now: number): void {
    const entry = this.entries.get(key);
    if (entry) entry.recheckAt = now + ISOLATE_RECHECK_MS;
  }

  delete(key: string): void { this.entries.delete(key); }

  clear(): void { this.entries.clear(); }
}
