// The agent view of a Scaffold session that is not running. Scaffold reads the chat's tail from the
// Crew Edge (`GET /chat2/<chatId>/tail`: the last messages its host published, kept while the
// session is paused) and asks this module, from its own image's Crew runtime, for the browser
// state. It is the live `/api/session` shape with the live entry bounds, so viewing a paused
// session needs no resume, and the same `browserEntry` keeps reasoning text out of the browser.
// Pure: no engine, no network. Scaffold owns authorization, the tail fetch and resuming.
import { browserEntry, HttpError } from './server.mjs';

export const DETACHED_CONTRACT = 'crew.native.web.detached.v1';

// Statuses Scaffold serves from the tail; a ready or agent_running session gets the live view.
const NOT_RUNNING = new Set(['creating', 'restoring_snapshot', 'starting', 'paused', 'resuming', 'stopped', 'failed']);
// Sending resumes these. A stopped session lost its workspace and a failed one cannot start.
const SENDS_RESUME = new Set(['creating', 'restoring_snapshot', 'starting', 'paused', 'resuming']);
const PAGE_ENTRIES = 100;
const PAGE_BYTES = 512 * 1024;
const TOOL_DETAIL_BYTES = 1024 * 1024;

function requireValue(ok, message) { if (!ok) throw new HttpError(400, message); }
function text(value, limit = 4096) { return typeof value === 'string' && value.length <= limit ? value : ''; }

function tailEntries(tail, chatId) {
  if (tail == null) return [];
  requireValue(typeof tail === 'object' && !Array.isArray(tail), 'Invalid Crew tail');
  // The tail is fetched by chat id; one for another chat must never render here.
  requireValue(tail.chatId === chatId, 'Crew tail belongs to another chat');
  requireValue(Array.isArray(tail.messages), 'Invalid Crew tail messages');
  for (const entry of tail.messages) {
    requireValue(entry && typeof entry.id === 'string' && typeof entry.role === 'string' && Array.isArray(entry.parts), 'Invalid Crew tail entry');
  }
  return tail.messages;
}

/**
 * `session`: `{ chatId, sandboxId, title, status, cwd?, branch?, model?, reasoning? }` from Scaffold's
 * record. `tail`: the Edge's tail JSON, or null when the host never published one.
 */
export function detachedState({ tail, session }) {
  requireValue(session && typeof session.chatId === 'string' && session.chatId.length > 0, 'Crew chat id required');
  requireValue(typeof session.sandboxId === 'string' && session.sandboxId.length > 0, 'Sandbox id required');
  requireValue(NOT_RUNNING.has(session.status), 'A running session is served live');
  const entries = tailEntries(tail, session.chatId);
  const messages = [];
  let size = 0;
  for (let index = entries.length - 1; index >= 0 && messages.length < PAGE_ENTRIES; index--) {
    const entry = entries[index];
    // Nothing streams while the session is not running: a turn the pause cut off was interrupted.
    const settled = entry.status === 'streaming' ? { ...entry, status: 'aborted' } : entry;
    const view = browserEntry(settled);
    const bytes = Buffer.byteLength(JSON.stringify(view));
    if (messages.length && size + bytes > PAGE_BYTES) break;
    messages.unshift(view);
    size += bytes;
  }
  const total = Number.isSafeInteger(tail?.totalMessages) && tail.totalMessages >= entries.length ? tail.totalMessages : entries.length;
  const sends = SENDS_RESUME.has(session.status);
  return {
    sandboxId: session.sandboxId,
    session: { id: session.chatId, title: text(session.title, 512) || 'Crew', cwd: text(session.cwd), branch: text(session.branch, 512),
      status: session.status, turnId: null, model: text(session.model, 256) || undefined, reasoning: text(session.reasoning, 64) || null },
    messages,
    models: [],
    // Older entries are not on the Edge in browser form: the page says so instead of offering a cursor.
    history: { hasOlder: false, before: null },
    transcriptReset: true,
    connection: 'paused',
    capabilities: { message: sends, input: false, interrupt: false },
    detached: { contract: DETACHED_CONTRACT, available: tail != null, shown: messages.length, total,
      updatedAt: Number.isSafeInteger(tail?.updatedAt) ? tail.updatedAt : null },
  };
}

/** The live view's tool detail for a tail part; the full output stays in the paused host's journal. */
export function detachedToolDetail({ tail, chatId, toolId }) {
  requireValue(typeof toolId === 'string' && toolId.length > 0 && toolId.length <= 200, 'Invalid tool id');
  const part = tailEntries(tail, chatId).flatMap((entry) => entry.parts).find((candidate) => candidate?.kind === 'tool' && candidate.id === toolId);
  if (!part) return { found: false };
  const detail = { found: true, resolved: part.resolved === true, isError: part.isError === true,
    input: JSON.stringify(part.call ?? null, null, 2), output: typeof part.output === 'string' ? part.output : '' };
  if (Buffer.byteLength(JSON.stringify(detail)) > TOOL_DETAIL_BYTES) return { found: true, resolved: detail.resolved, isError: detail.isError, input: null, output: null };
  return detail;
}
