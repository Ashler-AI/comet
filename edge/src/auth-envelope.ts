import type { ScaffoldAuthenticationResult } from "./auth";

// One isolate-wide budget, shared by dispatchers and every AuthGrant instance.
// Nested same-isolate dispatch consumes two slots, never a second budget.
// Dispatch uses at most half, leaving headroom for its same-isolate coordinators.
export const SCAFFOLD_TOKEN_BYTES = 8 * 1024;
export const SCAFFOLD_JSON_BYTES = 64 * 1024;
export const SCAFFOLD_ADMISSIONS = 32;
export const SCAFFOLD_DEADLINE_MS = 8_000;
let admitted = 0;
let dispatched = 0;

export const scaffoldTokenFits = (token: string): boolean =>
  token.length <= SCAFFOLD_TOKEN_BYTES && new TextEncoder().encode(token).byteLength <= SCAFFOLD_TOKEN_BYTES;

/** A stream limit, including chunked/expanded bytes, not a Content-Length trust check. */
export async function boundedAuthJson<T>(message: Request | Response, signal: AbortSignal): Promise<T> {
  const reader = message.body?.getReader();
  if (!reader) throw new Error("Missing authority body");
  let cancellation: Promise<void> | undefined;
  const cancel = () => { cancellation ??= reader.cancel().catch(() => {}); };
  signal.addEventListener("abort", cancel, { once: true });
  try {
    if (signal.aborted || Number(message.headers.get("content-length")) > SCAFFOLD_JSON_BYTES) {
      throw new Error("Authority body unavailable");
    }
    const bytes = new Uint8Array(SCAFFOLD_JSON_BYTES);
    let size = 0;
    while (true) {
      const { done, value } = await reader.read();
      signal.throwIfAborted();
      if (done) break;
      if (value.byteLength > bytes.byteLength - size) throw new Error("Authority body too large");
      bytes.set(value, size);
      size += value.byteLength;
    }
    return JSON.parse(new TextDecoder("utf-8", { fatal: true, ignoreBOM: false }).decode(bytes.subarray(0, size))) as T;
  } catch (error) {
    cancel();
    throw error;
  } finally {
    signal.removeEventListener("abort", cancel);
    // An uncooperative cancellation must keep its admission, not leave a dormant
    // reaction behind while cancellation churn admits another retained caller.
    if (cancellation) await cancellation;
    reader.releaseLock();
  }
}

/** Return promptly on abort, but charge the slot until ALL retained work settles. */
export async function scaffoldAdmission(
  kind: "dispatch" | "coordinator",
  signal: AbortSignal | undefined,
  operation: (signal: AbortSignal) => Promise<ScaffoldAuthenticationResult>
): Promise<ScaffoldAuthenticationResult> {
  if (signal?.aborted || admitted >= SCAFFOLD_ADMISSIONS || (kind === "dispatch" && dispatched >= SCAFFOLD_ADMISSIONS / 2)) {
    return { status: "unavailable", retryAfter: "1" };
  }
  admitted++;
  if (kind === "dispatch") dispatched++;
  const controller = new AbortController();
  const abort = () => { controller.abort(); };
  const timer = setTimeout(abort, SCAFFOLD_DEADLINE_MS);
  signal?.addEventListener("abort", abort, { once: true });
  let onAbort!: () => void;
  const cancelled = new Promise<ScaffoldAuthenticationResult>(resolve => {
    onAbort = () => { resolve({ status: "unavailable", retryAfter: "1" }); };
    controller.signal.addEventListener("abort", onAbort, { once: true });
  });
  const work = (async (): Promise<ScaffoldAuthenticationResult> => {
    try {
      return await operation(controller.signal);
    } catch {
      return { status: "unavailable", retryAfter: "1" };
    } finally {
      admitted--;
      if (kind === "dispatch") dispatched--;
    }
  })();
  try {
    return await Promise.race([work, cancelled]);
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", abort);
    controller.signal.removeEventListener("abort", onAbort);
  }
}
