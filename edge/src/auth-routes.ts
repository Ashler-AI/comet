import { bearerFromRequest, credentialTransportAllowed, requiredCapabilities, verifyScaffoldSessionResult, type ScaffoldAuthEnv, type ScaffoldAuthenticationResult, type ScaffoldVerification, type Verified } from "./auth";
import { AUTH_USER_HEADER, type Env } from "./env";
import { handleNativeGrantEpoch, nativeGrantEpoch, NATIVE_GRANT_ISSUER_HEADER, type NativeGrantEpoch } from "./native-grant-epoch";
import { boundedAuthJson, scaffoldAdmission, scaffoldTokenFits, SCAFFOLD_DEADLINE_MS, SCAFFOLD_JSON_BYTES } from "./auth-envelope";
import { boundedSession, ISOLATE_STALE_CHECK_MS, SessionCache, sessionPolicy, verifiedSession, type SessionPolicy, type VerifiedSession } from "./scaffold-session";

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const GRANT_TTL_MS = 15 * 60 * 1000;
const ACCESS_TTL_MS = 12 * 60 * 60 * 1000;
const SCAFFOLD_AUTHORITY_PROFILE_VERSION = "scaffold.comet-runtime.v1";

interface GrantRecord extends NativeGrantEpoch {
  grantId: string;
  email: string;
  capabilities: string[];
  grantHash: string;
  issuedAt: number;
  grantExpiresAt: number;
  consumedAt?: number;
  accessHash?: string;
  accessExpiresAt?: number;
  revokedAt?: number;
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" }
  });

const bodyJson = async <T>(request: Request | Response): Promise<T | undefined> => {
  try {
    return (await request.json()) as T;
  } catch {
    return undefined;
  }
};

const randomSecret = (): string =>
  `${crypto.randomUUID().replaceAll("-", "")}${crypto.randomUUID().replaceAll("-", "")}`;

const hash = async (value: string): Promise<string> => {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value));
  return btoa(String.fromCharCode(...new Uint8Array(digest)))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
};

/** A separate name space from the hexadecimal device-grant IDs; no token is stored. */
export const scaffoldVerificationName = async (env: ScaffoldAuthEnv, token: string): Promise<string> => {
  if (!scaffoldTokenFits(token)) throw new Error("Scaffold token too large");
  return `scaffold-verification:${await hash(JSON.stringify([
    token, env.SCAFFOLD_CONTROL_PLANE_URL, env.SCAFFOLD_PROJECT_SCOPE,
    [...requiredCapabilities(env)].sort(), env.ENVIRONMENT, env.SCAFFOLD_DEPLOYMENT_ID
  ]))}`;
};

/** Coordinator wire reply. Session bounds travel with every positive answer. */
type CoordinatorWire =
  | { status: "authenticated"; identity: Verified; verifiedAt: number; freshUntil: number; staleUntil: number }
  | { status: "invalid" }
  | { status: "denied" }
  | { status: "unavailable"; retryAfter?: string };

/** A validated coordinator reply. `session` is absent when nothing may be reused. */
type CoordinatorReply =
  | { status: "authenticated"; identity: Verified; session?: VerifiedSession }
  | { status: "invalid" }
  | { status: "denied" }
  | { status: "unavailable"; retryAfter?: string };

const sessionReply = (session: VerifiedSession): CoordinatorWire => ({
  status: "authenticated", identity: session.identity,
  verifiedAt: session.verifiedAt, freshUntil: session.freshUntil, staleUntil: session.staleUntil
});

const fitsEnvelope = (value: unknown): boolean => {
  const encoded = JSON.stringify(value);
  return encoded.length <= SCAFFOLD_JSON_BYTES && new TextEncoder().encode(encoded).byteLength <= SCAFFOLD_JSON_BYTES;
};

const coordinatorIdentity = (env: ScaffoldAuthEnv, identity: unknown): identity is Verified => {
  if (!identity || typeof identity !== "object") return false;
  const value = identity as Partial<Verified>;
  return value.credential === "scaffold" &&
    typeof value.userId === "string" && value.userId.length > 0 &&
    typeof value.email === "string" && value.email.length > 0 &&
    value.projectScope === env.SCAFFOLD_PROJECT_SCOPE.trim() &&
    value.orgId === value.projectScope &&
    Array.isArray(value.capabilities) &&
    value.capabilities.every((capability) => typeof capability === "string") &&
    requiredCapabilities(env).every((capability) => value.capabilities!.includes(capability));
};

const coordinatorReply = (env: ScaffoldAuthEnv, value: unknown, policy: SessionPolicy): CoordinatorReply => {
  if (!value || typeof value !== "object") return { status: "unavailable" };
  const reply = value as Record<string, unknown>;
  if (reply.status === "invalid") return { status: "invalid" };
  if (reply.status === "denied") return { status: "denied" };
  if (reply.status === "unavailable") {
    return typeof reply.retryAfter === "string" ? { status: "unavailable", retryAfter: reply.retryAfter } : { status: "unavailable" };
  }
  const identity = reply.identity;
  if (reply.status !== "authenticated" || !coordinatorIdentity(env, identity)) return { status: "unavailable" };
  // A coordinator without bounds (mid-deploy) authorizes this request and nothing more.
  const session = boundedSession({
    identity, verifiedAt: reply.verifiedAt, freshUntil: reply.freshUntil, staleUntil: reply.staleUntil
  }, policy, Date.now());
  return session ? { status: "authenticated", identity, session } : { status: "authenticated", identity };
};

/** `denied` keeps today's public answer (503); only its effect on reuse differs. */
const publicResult = (reply: CoordinatorReply): ScaffoldAuthenticationResult => {
  switch (reply.status) {
    case "authenticated": return { status: "authenticated", identity: reply.identity };
    case "invalid": return { status: "invalid" };
    case "denied": return { status: "unavailable" };
    case "unavailable":
      return reply.retryAfter === undefined ? { status: "unavailable" } : { status: "unavailable", retryAfter: reply.retryAfter };
  }
};

/** Per-isolate reuse, keyed by coordinator name (token + full authority configuration). */
const isolateSessions = new SessionCache<CoordinatorReply>();
/** Test seam: isolates are long-lived, test cases are not. */
export const resetIsolateScaffoldSessions = (): void => { isolateSessions.clear(); };
export const isolateScaffoldSessionCount = (): number => isolateSessions.size;

const remember = (name: string, reply: CoordinatorReply, policy: SessionPolicy): void => {
  const now = Date.now();
  if (reply.status === "unavailable") isolateSessions.deferRecheck(name, now);
  else if (reply.status === "authenticated" && reply.session) isolateSessions.put(name, reply.session, now, policy);
  else isolateSessions.delete(name);
};

const consultCoordinator = (
  env: ScaffoldAuthEnv, name: string, token: string, signal: AbortSignal | undefined, deadlineMs: number, policy: SessionPolicy
): Promise<CoordinatorReply> => scaffoldAdmission("dispatch", signal, async (signal): Promise<CoordinatorReply> => {
  const response = await env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(name)).fetch("https://grant.internal/verify-scaffold", {
    method: "POST", body: JSON.stringify({ token }), signal
  });
  if (!response.ok) {
    await response.body?.cancel();
    return { status: "unavailable" };
  }
  return coordinatorReply(env, await boundedAuthJson<unknown>(response, signal), policy);
}, deadlineMs);

const settledOrAborted = <T>(promise: Promise<T>, signal?: AbortSignal): Promise<T | undefined> => {
  if (!signal) return promise;
  if (signal.aborted) return Promise.resolve(undefined);
  return new Promise<T | undefined>(resolve => {
    const abort = () => { resolve(undefined); };
    signal.addEventListener("abort", abort, { once: true });
    void promise.then(value => {
      signal.removeEventListener("abort", abort);
      resolve(value);
    });
  });
};

/**
 * Fresh reuse costs no hop. Past fresh, one short coordinator consultation per
 * isolate and token decides; the stale entry answers only when the authority
 * cannot. Revocation (401/403) and definitive denial purge the entry.
 */
export const verifyScaffoldThroughCoordinator = async (
  env: ScaffoldAuthEnv, token: string, signal?: AbortSignal
): Promise<ScaffoldAuthenticationResult> => {
  if (!scaffoldTokenFits(token)) return { status: "unavailable", retryAfter: "1" };
  let name: string;
  try {
    name = await scaffoldVerificationName(env, token);
  } catch {
    return { status: "unavailable", retryAfter: "1" };
  }
  const policy = sessionPolicy(env);
  const entry = isolateSessions.get(name, Date.now());
  if (!entry) {
    const reply = await consultCoordinator(env, name, token, signal, SCAFFOLD_DEADLINE_MS, policy);
    remember(name, reply, policy);
    return publicResult(reply);
  }
  const now = Date.now();
  if (now < entry.session.freshUntil || now < entry.recheckAt) {
    return { status: "authenticated", identity: entry.session.identity };
  }
  // Shared by this isolate's callers and bounded on its own, never by one caller.
  const check = entry.check ??= (async () => {
    try {
      const reply = await consultCoordinator(env, name, token, undefined, ISOLATE_STALE_CHECK_MS, policy);
      remember(name, reply, policy);
      return reply;
    } finally {
      entry.check = undefined;
    }
  })();
  const reply = await settledOrAborted(check, signal);
  if (reply === undefined) return { status: "unavailable", retryAfter: "1" };
  if (reply.status !== "unavailable") return publicResult(reply);
  const fallback = isolateSessions.get(name, Date.now());
  return fallback ? { status: "authenticated", identity: fallback.session.identity } : publicResult(reply);
};

export const parseGrantToken = (
  value: string,
  prefix: "cg1" | "cs1"
): { id: string; secret: string } | undefined => {
  const [actualPrefix, id, secret, extra] = value.split(".");
  if (actualPrefix !== prefix || extra !== undefined || !id || !secret) return undefined;
  if (!/^[a-f0-9]{32}$/.test(id) || !/^[a-f0-9]{64}$/.test(secret)) return undefined;
  return { id, secret };
};

const grantStub = (env: Env, id: string): DurableObjectStub =>
  env.AUTH_GRANTS.get(env.AUTH_GRANTS.idFromName(id));

const SCAFFOLD_SESSION_KEY = "scaffold-session";
/** A coordinator past fresh waits this long for Scaffold before answering from its record. */
const COORDINATOR_STALE_WAIT_MS = 1_000;
/** Collapses a burst of checks for a just-rejected token; never longer than this. */
const REJECTION_HOLD_MS = 2_000;

interface Refresh {
  readonly promise: Promise<CoordinatorWire>;
  readonly controller: AbortController;
  /** A refresh of a reusable record outlives its callers; a cold check stops when all leave. */
  detached: boolean;
  waiters: number;
}

const settledWithin = <T>(promise: Promise<T>, ms: number, signal: AbortSignal): Promise<T | undefined> => {
  if (signal.aborted) return Promise.resolve(undefined);
  return new Promise<T | undefined>(resolve => {
    const done = (value: T | undefined) => {
      clearTimeout(timer);
      signal.removeEventListener("abort", expire);
      resolve(value);
    };
    const expire = () => { done(undefined); };
    const timer = setTimeout(expire, ms);
    signal.addEventListener("abort", expire, { once: true });
    promise.then(done, expire);
  });
};

/**
 * Grant storage stays atomic. A prefixed verification coordinator (one per
 * token and configuration) keeps one bounded session record, single-flight
 * refresh, a short rejection hold and the upstream cooldown.
 */
export class AuthGrant implements DurableObject {
  private session?: VerifiedSession;
  private sessionLoad?: Promise<void>;
  private rejection?: { status: "invalid" | "denied"; until: number };
  private refresh?: Refresh;
  private unavailableUntil = 0;
  private unavailableRetryAfter?: string;

  constructor(private readonly ctx: DurableObjectState, private readonly env: Env) {}

  async fetch(request: Request): Promise<Response> {
    // Do not hold the storage gate here: other isolates must join pending verification.
    if (new URL(request.url).pathname === "/verify-scaffold") {
      if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
      const result = await scaffoldAdmission("coordinator", request.signal, async (signal): Promise<CoordinatorWire> => {
        const body = await boundedAuthJson<{ token?: unknown }>(request, signal);
        if (typeof body?.token !== "string" || !body.token.startsWith("sc_rc_")) return { status: "invalid" };
        if (!scaffoldTokenFits(body.token)) return { status: "unavailable", retryAfter: "1" };
        const name = await scaffoldVerificationName(this.env, body.token);
        signal.throwIfAborted();
        if (this.ctx.id.toString() !== this.env.AUTH_GRANTS.idFromName(name).toString()) {
          return { status: "unavailable" };
        }
        return this.verify(body.token, signal);
      });
      // Subjects appear twice; cooldown headers also pass this output envelope.
      // Keep the original upstream cooldown even if its reply cannot be emitted.
      if (!fitsEnvelope(result)) return json({ status: "unavailable", retryAfter: "1" });
      return new Response(JSON.stringify(result), { headers: { "content-type": "application/json", "cache-control": "no-store" } });
    }
    return this.ctx.blockConcurrencyWhile(() => this.handle(request));
  }

  /** Only verification coordinators schedule alarms: drop a record once its grace bound passes. */
  async alarm(): Promise<void> {
    const now = Date.now();
    const stored = boundedSession(await this.ctx.storage.get(SCAFFOLD_SESSION_KEY), sessionPolicy(this.env), now);
    if (stored && now < stored.staleUntil) {
      await this.ctx.storage.setAlarm(stored.staleUntil);
      return;
    }
    await this.ctx.storage.delete(SCAFFOLD_SESSION_KEY);
    if (this.session && now >= this.session.staleUntil) this.session = undefined;
  }

  private async verify(token: string, signal: AbortSignal): Promise<CoordinatorWire> {
    const session = await this.reusableSession();
    const now = Date.now();
    if (this.rejection && now < this.rejection.until) return { status: this.rejection.status };
    if (session && now < session.freshUntil) return sessionReply(session);
    if (session) {
      // Past fresh, Scaffold decides whenever it answers in time. The record is
      // only the unavailable-authority fallback, and never past its grace bound.
      if (this.unavailableUntil <= now) {
        const reply = await settledWithin(this.startRefresh(token, true).promise, COORDINATOR_STALE_WAIT_MS, signal);
        if (reply && reply.status !== "unavailable") return reply;
      }
      const current = await this.reusableSession();
      if (current) return sessionReply(current);
      if (this.rejection && Date.now() < this.rejection.until) return { status: this.rejection.status };
      return this.cooldown() ?? { status: "unavailable", retryAfter: "1" };
    }
    return this.cooldown() ?? this.join(this.startRefresh(token, false), signal);
  }

  private cooldown(): { status: "unavailable"; retryAfter: string } | undefined {
    const now = Date.now();
    if (this.unavailableUntil <= now) return undefined;
    return { status: "unavailable", retryAfter: this.unavailableRetryAfter ?? String(Math.ceil((this.unavailableUntil - now) / 1000)) };
  }

  private async join(refresh: Refresh, signal: AbortSignal): Promise<CoordinatorWire> {
    refresh.waiters++;
    let waiting = true;
    const leave = () => {
      if (!waiting) return;
      waiting = false;
      refresh.waiters--;
      if (refresh.waiters === 0 && !refresh.detached) refresh.controller.abort();
    };
    signal.addEventListener("abort", leave, { once: true });
    try {
      // Do not race here: the isolate admission remains charged for this
      // continuation until shared work settles, even after the caller leaves.
      return await refresh.promise;
    } finally {
      signal.removeEventListener("abort", leave);
      leave();
    }
  }

  /** One Scaffold check per coordinator at a time, bounded by the shared deadline. */
  private startRefresh(token: string, detached: boolean): Refresh {
    if (this.refresh) {
      this.refresh.detached ||= detached;
      return this.refresh;
    }
    const controller = new AbortController();
    let expired = false;
    const timer = setTimeout(() => { expired = true; controller.abort(); }, SCAFFOLD_DEADLINE_MS);
    const promise: Promise<CoordinatorWire> = (async (): Promise<CoordinatorWire> => {
      let result: ScaffoldVerification;
      try {
        result = await verifyScaffoldSessionResult(this.env, token, controller.signal);
      } catch {
        result = { status: "unavailable", retryAfter: "1" };
      }
      // Cancelled by its last caller: no cooldown, as before. An expired deadline is an authority answer.
      if (controller.signal.aborted && !expired) return { status: "unavailable", retryAfter: "1" };
      return this.record(expired ? { status: "unavailable", retryAfter: "1" } : result);
    })().finally(() => {
      clearTimeout(timer);
      if (this.refresh?.promise === promise) this.refresh = undefined;
    });
    this.refresh = { promise, controller, detached, waiters: 0 };
    this.ctx.waitUntil?.(promise);
    return this.refresh;
  }

  private async record(result: ScaffoldVerification): Promise<CoordinatorWire> {
    const now = Date.now();
    if (result.status === "authenticated") {
      const session = verifiedSession(result.identity, now, sessionPolicy(this.env));
      const reply = sessionReply(session);
      // An answer the coordinator envelope cannot carry is neither published nor reused.
      if (!fitsEnvelope(reply)) return { status: "unavailable", retryAfter: "1" };
      this.session = session;
      this.rejection = undefined;
      this.unavailableUntil = 0;
      this.unavailableRetryAfter = undefined;
      await this.store(session);
      return reply;
    }
    if (result.status === "invalid" || result.status === "denied") {
      this.session = undefined;
      this.rejection = { status: result.status, until: now + REJECTION_HOLD_MS };
      await this.store(undefined);
      return { status: result.status };
    }
    const value = result.retryAfter?.trim();
    const numeric = value !== undefined && /^\d+$/.test(value);
    const deadline = numeric ? now + Number(value) * 1000 : value ? Date.parse(value) : NaN;
    // Unrepresentable valid delays must not become an early retry or an invalid Infinity header.
    this.unavailableUntil = numeric && !Number.isSafeInteger(deadline) ? Infinity
      : Number.isFinite(deadline) ? Math.max(now, deadline) : now + 1000;
    this.unavailableRetryAfter = this.unavailableUntil === Infinity ? value : undefined;
    // One fixed-field event per failed refresh (no subject or token): visible in `wrangler tail`.
    if (this.session && now < this.session.staleUntil) {
      console.warn("scaffold auth unavailable; reusing verified session", {
        graceRemainingSeconds: Math.floor((this.session.staleUntil - now) / 1000)
      });
    }
    return result;
  }

  private async reusableSession(): Promise<VerifiedSession | undefined> {
    this.sessionLoad ??= (async () => {
      try {
        const stored = boundedSession(await this.ctx.storage.get(SCAFFOLD_SESSION_KEY), sessionPolicy(this.env), Date.now());
        if (!this.session && stored?.identity.projectScope === this.env.SCAFFOLD_PROJECT_SCOPE.trim()) this.session = stored;
      } catch {
        // Start without reuse; Scaffold remains the authority.
      }
    })();
    await this.sessionLoad;
    if (this.session && Date.now() >= this.session.staleUntil) this.session = undefined;
    return this.session;
  }

  /** Storage only carries reuse across coordinator restarts; memory already bounds it. */
  private async store(session: VerifiedSession | undefined): Promise<void> {
    try {
      if (session) {
        await this.ctx.storage.put(SCAFFOLD_SESSION_KEY, session);
        await this.ctx.storage.setAlarm(session.staleUntil);
      } else {
        await this.ctx.storage.delete(SCAFFOLD_SESSION_KEY);
        await this.ctx.storage.deleteAlarm();
      }
    } catch {
      // A failed write never widens reuse: the memory record is authoritative here.
    }
  }

  private async handle(request: Request): Promise<Response> {
    const path = new URL(request.url).pathname;
    const epochs = this.env.NATIVE_GRANT_EPOCHS;
    if ("NATIVE_GRANT_EPOCHS" in this.env && !epochs) {
      return json({ error: "epoch_authority_unavailable" }, 503);
    }
    if (epochs) {
      const response = await handleNativeGrantEpoch(this.ctx, this.env, request);
      if (response) return response;
    }
    if (path === "/issue" && request.method === "POST") {
      const record = await bodyJson<GrantRecord>(request);
      if (!record || (await this.ctx.storage.get<GrantRecord>("grant"))) {
        return json({ error: "grant_conflict" }, 409);
      }
      if (epochs) {
        // Public issuance reaches this call only after verifiedSandboxTarget and
        // ownership checks; no client header or unverified target is forwarded.
        if (request.headers.get(NATIVE_GRANT_ISSUER_HEADER) !== "1" ||
            !/^[a-f0-9]{32}$/.test(record.grantId) ||
            this.env.AUTH_GRANTS.idFromName(record.grantId).toString() !== this.ctx.id.toString()) {
          return json({ error: "forbidden" }, 403);
        }
        if (!(await nativeGrantEpoch(epochs, record, "advance"))) return json({ error: "stale_epoch" }, 409);
      }
      await this.ctx.storage.put("grant", record);
      // A newer verified issuance may overtake the local store. Never publish
      // an older grant as usable, and never roll its coordinator backward.
      if (epochs && !(await nativeGrantEpoch(epochs, record, "check"))) return json({ error: "stale_epoch" }, 409);
      return json({ ok: true });
    }

    const record = await this.ctx.storage.get<GrantRecord>("grant");
    if (!record || record.revokedAt) return json({ error: "invalid_grant" }, 401);
    if (epochs && (path === "/exchange" || path === "/refresh" || path === "/authenticate") &&
        !(await nativeGrantEpoch(epochs, record, "check"))) {
      return json({ error: "invalid_grant" }, 401);
    }

    if (path === "/exchange" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        record.consumedAt ||
        record.grantExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.grantHash
      ) {
        return json({ error: "invalid_grant" }, 401);
      }
      const secret = randomSecret();
      record.consumedAt = Date.now();
      record.accessHash = await hash(secret);
      record.accessExpiresAt = Date.now() + ACCESS_TTL_MS;
      await this.ctx.storage.put("grant", record);
      if (epochs && !(await nativeGrantEpoch(epochs, record, "check"))) return json({ error: "invalid_grant" }, 401);
      return json({
        accessToken: `cs1.${record.grantId}.${secret}`,
        expiresAt: record.accessExpiresAt,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities
      });
    }

    if (path === "/refresh" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        !record.accessHash ||
        !record.accessExpiresAt ||
        record.accessExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.accessHash
      ) {
        return json({ error: "invalid_token" }, 401);
      }
      const secret = randomSecret();
      record.accessHash = await hash(secret);
      record.accessExpiresAt = Date.now() + ACCESS_TTL_MS;
      await this.ctx.storage.put("grant", record);
      if (epochs && !(await nativeGrantEpoch(epochs, record, "check"))) return json({ error: "invalid_token" }, 401);
      return json({
        accessToken: `cs1.${record.grantId}.${secret}`,
        expiresAt: record.accessExpiresAt,
        grantId: record.grantId,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities
      });
    }

    if (path === "/authenticate" && request.method === "POST") {
      const body = await bodyJson<{ secret?: string }>(request);
      if (
        typeof body?.secret !== "string" ||
        !record.accessHash ||
        !record.accessExpiresAt ||
        record.accessExpiresAt <= Date.now() ||
        (await hash(body.secret)) !== record.accessHash
      ) {
        return json({ error: "invalid_token" }, 401);
      }
      if (epochs && !(await nativeGrantEpoch(epochs, record, "check"))) return json({ error: "invalid_token" }, 401);
      return json({
        grantId: record.grantId,
        userId: record.ownerUserId,
        email: record.email,
        projectId: record.projectId,
        deploymentId: record.deploymentId,
        sandboxId: record.sandboxId,
        targetDeviceId: record.targetDeviceId,
        sessionId: record.sessionId,
        lifecycleEpoch: record.lifecycleEpoch,
        capabilities: record.capabilities,
        grantedAt: record.issuedAt,
        expiresAt: record.accessExpiresAt,
        revokedAt: null
      });
    }

    if (path === "/revoke" && request.method === "POST") {
      const body = await bodyJson<{ ownerUserId?: string }>(request);
      if (body?.ownerUserId !== record.ownerUserId) return json({ error: "forbidden" }, 403);
      record.revokedAt = Date.now();
      await this.ctx.storage.put("grant", record);
      return json({ ok: true });
    }
    return json({ error: "not_found" }, 404);
  }
}

export const authenticateDeviceToken = async (
  env: Env,
  token: string
): Promise<(Verified & {
  grantId: string;
  projectId: string;
  deploymentId: string;
  sandboxId: string;
  targetDeviceId: string;
  sessionId: string;
  lifecycleEpoch: number;
  grantedAt: number;
  expiresAt: number;
  revokedAt: null;
}) | undefined> => {
  const parsed = parseGrantToken(token, "cs1");
  if (!parsed) return undefined;
  const response = await grantStub(env, parsed.id).fetch("https://grant.internal/authenticate", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
  if (response.status === 401 || response.status === 403) return undefined;
  if (!response.ok) throw new Error("grant authority unavailable");
  const body = await bodyJson<{
    userId?: unknown;
    email?: unknown;
    grantId?: unknown;
    projectId?: unknown;
    deploymentId?: unknown;
    sandboxId?: unknown;
    targetDeviceId?: unknown;
    sessionId?: unknown;
    lifecycleEpoch?: unknown;
    capabilities?: unknown;
    grantedAt?: unknown;
    expiresAt?: unknown;
    revokedAt?: unknown;
  }>(response);
  if (
    typeof body?.userId !== "string" ||
    typeof body.email !== "string" ||
    typeof body.projectId !== "string" ||
    body.projectId !== env.SCAFFOLD_PROJECT_SCOPE ||
    typeof body.deploymentId !== "string" ||
    body.deploymentId !== env.SCAFFOLD_DEPLOYMENT_ID ||
    typeof body.sandboxId !== "string" ||
    typeof body.grantId !== "string" ||
    typeof body.targetDeviceId !== "string" ||
    typeof body.sessionId !== "string" ||
    typeof body.lifecycleEpoch !== "number" ||
    !Number.isSafeInteger(body.lifecycleEpoch) ||
    body.lifecycleEpoch < 1 ||
    typeof body.grantedAt !== "number" ||
    !Number.isSafeInteger(body.grantedAt) ||
    typeof body.expiresAt !== "number" ||
    !Number.isSafeInteger(body.expiresAt) ||
    body.expiresAt <= Date.now() ||
    body.revokedAt !== null ||
    !Array.isArray(body.capabilities) ||
    !body.capabilities.every((value): value is string => typeof value === "string")
  ) {
    return undefined;
  }
  return {
    userId: body.userId,
    email: body.email,
    projectScope: body.projectId,
    grantId: body.grantId,
    projectId: body.projectId,
    deploymentId: body.deploymentId,
    sandboxId: body.sandboxId,
    targetDeviceId: body.targetDeviceId,
    sessionId: body.sessionId,
    lifecycleEpoch: body.lifecycleEpoch,
    grantedAt: body.grantedAt,
    expiresAt: body.expiresAt,
    revokedAt: null,
    capabilities: body.capabilities,
    credential: "device"
  };
};

/** Public one-time exchange. No other auth route is allowed before authentication. */
export const handlePublicAuthRoute = async (
  request: Request,
  env: Env,
  url: URL
): Promise<Response | undefined> => {
  if (url.pathname !== "/auth/device-grants/exchange") return undefined;
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
  const body = await bodyJson<{ grant?: string }>(request);
  const parsed = typeof body?.grant === "string" ? parseGrantToken(body.grant, "cg1") : undefined;
  if (!parsed) return json({ error: "invalid_grant" }, 401);
  return grantStub(env, parsed.id).fetch("https://grant.internal/exchange", {
    method: "POST",
    body: JSON.stringify({ secret: parsed.secret })
  });
};

interface SandboxTarget {
  projectId: string;
  sandboxId: string;
  deploymentId: string;
  targetDeviceId: string;
  sessionId: string;
  lifecycleEpoch: number;
}

const verifiedSandboxTarget = async (
  request: Request,
  env: Env,
  identity: Verified,
  requested: SandboxTarget
): Promise<SandboxTarget | undefined> => {
  const bearer = bearerFromRequest(request);
  if (!bearer || identity.credential !== "scaffold") return undefined;
  let origin: string;
  try {
    const configured = new URL(env.SCAFFOLD_CONTROL_PLANE_URL);
    if (
      !credentialTransportAllowed(configured) ||
      !["http:", "https:"].includes(configured.protocol) ||
      !["", "/"].includes(configured.pathname) ||
      configured.search ||
      configured.hash
    ) {
      return undefined;
    }
    origin = configured.origin;
  } catch {
    return undefined;
  }
  let response: Response;
  try {
    response = await fetch(
      `${origin}/api/code-sandboxes/${encodeURIComponent(requested.sandboxId)}/comet-target/verify`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${bearer}`,
          accept: "application/json",
          "content-type": "application/json"
        },
        body: JSON.stringify(requested),
        signal: AbortSignal.timeout(5000),
        redirect: "manual"
      }
    );
  } catch {
    throw new Error("sandbox authority unavailable");
  }
  if (response.status === 401 || response.status === 403 || response.status === 404) return undefined;
  if (!response.ok) throw new Error("sandbox authority unavailable");
  const payload = await bodyJson<{
    ok?: unknown;
    profile?: {
      version?: unknown;
      projectId?: unknown;
      sandboxId?: unknown;
      deploymentId?: unknown;
      targetDeviceId?: unknown;
      sessionId?: unknown;
      lifecycleEpoch?: unknown;
      actor?: { sub?: unknown };
    };
  }>(response);
  const profile = payload?.profile;
  const actorSubject =
    typeof profile?.actor?.sub === "string" ? profile.actor.sub.trim().toLowerCase() : "";
  if (
    payload?.ok !== true ||
    profile?.version !== SCAFFOLD_AUTHORITY_PROFILE_VERSION ||
    actorSubject !== identity.userId ||
    typeof profile.projectId !== "string" ||
    profile.projectId !== requested.projectId ||
    profile.projectId !== identity.projectScope ||
    typeof profile.sandboxId !== "string" ||
    !ID_RE.test(profile.sandboxId) ||
    profile.sandboxId !== requested.sandboxId ||
    typeof profile.deploymentId !== "string" ||
    !ID_RE.test(profile.deploymentId) ||
    profile.deploymentId !== requested.deploymentId ||
    typeof profile.targetDeviceId !== "string" ||
    !ID_RE.test(profile.targetDeviceId) ||
    typeof profile.lifecycleEpoch !== "number" ||
    !Number.isSafeInteger(profile.lifecycleEpoch) ||
    profile.lifecycleEpoch !== requested.lifecycleEpoch ||
    profile.targetDeviceId !== requested.targetDeviceId ||
    typeof profile.sessionId !== "string" ||
    !ID_RE.test(profile.sessionId) ||
    profile.sessionId !== requested.sessionId
  ) {
    return undefined;
  }
  return {
    projectId: profile.projectId,
    sandboxId: profile.sandboxId,
    deploymentId: profile.deploymentId,
    targetDeviceId: profile.targetDeviceId,
    lifecycleEpoch: profile.lifecycleEpoch,
    sessionId: profile.sessionId
  };
};

const requesterOwnsSession = async (
  env: Env,
  target: Pick<SandboxTarget, "projectId" | "deploymentId" | "sessionId">,
  userId: string
): Promise<boolean> => {
  const room = env.CHAT_ROOMS.get(env.CHAT_ROOMS.idFromName(`chat2/${target.sessionId}`));
  const response = await room.fetch(new Request("https://chat.internal/bind-owner", {
    method: "POST",
    headers: { [AUTH_USER_HEADER]: userId }
  }));
  if (response.status === 403) return false;
  if (!response.ok) throw new Error("chat authority unavailable");
  return true;
};

export const handleAuthenticatedAuthRoute = async (
  request: Request,
  env: Env,
  url: URL,
  identity: Verified
): Promise<Response | undefined> => {
  if (!credentialTransportAllowed(url)) return json({ error: "secure_transport_required" }, 403);
  if (url.pathname === "/auth/device-token/refresh") {
    if (identity.credential !== "device") return json({ error: "forbidden" }, 403);
    if (request.method !== "POST") return json({ error: "method_not_allowed" }, 405);
    const bearer = bearerFromRequest(request);
    const parsed = bearer ? parseGrantToken(bearer, "cs1") : undefined;
    const identityGrantId = "grantId" in identity ? identity.grantId : undefined;
    if (!parsed || typeof identityGrantId !== "string" || parsed.id !== identityGrantId) {
      return json({ error: "forbidden" }, 403);
    }
    return grantStub(env, parsed.id).fetch("https://grant.internal/refresh", {
      method: "POST",
      body: JSON.stringify({ secret: parsed.secret })
    });
  }
  if (url.pathname !== "/auth/device-grants") return undefined;
  if (identity.credential === "device") return json({ error: "forbidden" }, 403);

  if (request.method === "POST") {
    const body = await bodyJson<{
      deploymentId?: string;
      sandboxId?: string;
      targetDeviceId?: string;
      sessionId?: string;
      lifecycleEpoch?: number;
      capabilities?: string[];
      ttlSeconds?: number;
    }>(request);
    if (
      !body ||
      typeof body.deploymentId !== "string" ||
      !ID_RE.test(body.deploymentId) ||
      body.deploymentId !== env.SCAFFOLD_DEPLOYMENT_ID ||
      typeof body.sandboxId !== "string" ||
      !ID_RE.test(body.sandboxId) ||
      typeof body.targetDeviceId !== "string" ||
      !ID_RE.test(body.targetDeviceId) ||
      typeof body.sessionId !== "string" ||
      typeof body.lifecycleEpoch !== "number" ||
      !Number.isSafeInteger(body.lifecycleEpoch) ||
      body.lifecycleEpoch < 1 ||
      !ID_RE.test(body.sessionId) ||
      !Array.isArray(body.capabilities) ||
      (body.ttlSeconds !== undefined &&
        (typeof body.ttlSeconds !== "number" || !Number.isFinite(body.ttlSeconds))) ||
      body.capabilities.length === 0 ||
      !body.capabilities.every(
        (capability) => typeof capability === "string" && identity.capabilities.includes(capability)
      )
    ) {
      return json({ error: "invalid_grant_request" }, 400);
    }
    const target = await verifiedSandboxTarget(request, env, identity, {
      projectId: identity.projectScope,
      deploymentId: body.deploymentId,
      sandboxId: body.sandboxId,
      targetDeviceId: body.targetDeviceId,
      sessionId: body.sessionId,
      lifecycleEpoch: body.lifecycleEpoch
    });
    if (!target) return json({ error: "grant_target_forbidden" }, 403);
    if (!(await requesterOwnsSession(env, target, identity.userId))) {
      return json({ error: "grant_target_forbidden" }, 403);
    }
    const id = crypto.randomUUID().replaceAll("-", "");
    const secret = randomSecret();
    const ttl = Math.min(
      GRANT_TTL_MS,
      Math.max(60_000, Math.floor((body.ttlSeconds ?? GRANT_TTL_MS / 1000) * 1000))
    );
    const issuedAt = Date.now();
    const expiresAt = issuedAt + ttl;
    const record: GrantRecord = {
      grantId: id,
      ownerUserId: identity.userId,
      email: identity.email,
      projectId: identity.projectScope,
      deploymentId: target.deploymentId,
      sandboxId: target.sandboxId,
      targetDeviceId: target.targetDeviceId,
      sessionId: target.sessionId,
      lifecycleEpoch: target.lifecycleEpoch,
      capabilities: [...new Set(body.capabilities)],
      grantHash: await hash(secret),
      issuedAt,
      grantExpiresAt: expiresAt
    };
    const response = await grantStub(env, id).fetch("https://grant.internal/issue", {
      method: "POST",
      headers: env.NATIVE_GRANT_EPOCHS ? { [NATIVE_GRANT_ISSUER_HEADER]: "1" } : undefined,
      body: JSON.stringify(record)
    });
    if (!response.ok) return json({ error: "grant_store_unavailable" }, 503);
    return json({
      grant: `cg1.${id}.${secret}`,
      expiresAt,
      accessExpiresAt: issuedAt + ACCESS_TTL_MS
    });
  }

  if (request.method === "DELETE") {
    const grantId = url.searchParams.get("id") ?? "";
    if (!/^[a-f0-9]{32}$/.test(grantId)) return json({ error: "invalid_grant_id" }, 400);
    return grantStub(env, grantId).fetch("https://grant.internal/revoke", {
      method: "POST",
      body: JSON.stringify({ ownerUserId: identity.userId })
    });
  }
  return json({ error: "method_not_allowed" }, 405);
};
