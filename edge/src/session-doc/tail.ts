/**
 * Tail materialization for the session doc, without a Mirror. Project only
 * the transcript and its identifying metadata, not unrelated doc roots.
 */
import { LoroDoc } from "loro-crdt";
import { SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT } from "./constants";
import { joinContinuations, type SessionMessageEntry } from "./messages";
import type { SessionTail } from "./sidecar";

/** Read the doc's message entries without projecting unrelated containers. */
export const readMessageEntries = (doc: LoroDoc): ReadonlyArray<SessionMessageEntry> => {
  const messages = doc.getList("messages");
  try {
    return messages.toJSON() as SessionMessageEntry[];
  } finally {
    messages.free();
  }
};

/** Materialize the DO's `tail` slot (§5 L2): last-N messages with
 * continuations joined, plus enough meta for the client to render instantly
 * and know how much history the full sync will bring. */
export const materializeTail = (
  doc: LoroDoc,
  now: number,
  tailCount: number = TAIL_MESSAGE_COUNT
): SessionTail => {
  const all = joinContinuations(readMessageEntries(doc));
  const meta = doc.getMap("meta");
  try {
    return {
      chatId: (meta.get("chatId") as string | undefined) ?? "",
      schemaVersion: (meta.get("schemaVersion") as number | undefined) ?? SESSION_SCHEMA_VERSION,
      messages: all.slice(-tailCount),
      totalMessages: all.length,
      updatedAt: now
    };
  } finally {
    meta.free();
  }
};
