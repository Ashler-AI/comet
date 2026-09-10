import { LoroDoc, LoroList, LoroMap } from "loro-crdt";
import { describe, expect, it } from "vitest";
import {
  isPeerMessageEntry,
  joinContinuations,
  splitMessageEntry
} from "./messages";
import type { SessionMessageEntry } from "./messages";
import { materializeTail, readMessageEntries } from "./tail";

const peer = { commandId: "peer-1", sourceChatId: "source", threadId: "thread", replyTo: "earlier" };
const entry: SessionMessageEntry = {
  id: peer.commandId,
  role: "user",
  parts: [{ kind: "text", id: "text", text: "Original peer body\nincluding its full second line." }],
  createdAt: 123,
  deviceId: "host",
  status: "complete",
  peerMessage: peer
};

// Use the native lists-of-maps layout with scalar provenance, not a mock doc.
const appendEntry = (doc: LoroDoc, value: SessionMessageEntry): LoroMap => {
  const messages = doc.getList("messages");
  const message = messages.insertContainer(messages.length, new LoroMap());
  const { parts, ...fields } = value;
  for (const [key, field] of Object.entries(fields)) {
    if (field !== undefined) message.set(key, field);
  }
  const list = message.setContainer("parts", new LoroList());
  for (const part of parts) {
    const map = list.insertContainer(list.length, new LoroMap());
    for (const [key, field] of Object.entries(part)) {
      if (field !== undefined) map.set(key, field);
    }
  }
  return message;
};

const textOf = (value: SessionMessageEntry): string =>
  value.parts.map((part) => part.text ?? "").join("");

// Runtime values can precede this client's wire schema.
const withMetadata = (metadata: unknown): SessionMessageEntry =>
  ({ ...entry, peerMessage: metadata }) as SessionMessageEntry;

describe("peer message provenance", () => {
  it("hides only valid native user identity, never legacy text or unsupported metadata", () => {
    expect(isPeerMessageEntry(entry)).toBe(true);
    expect(isPeerMessageEntry(withMetadata({ ...peer, replyTo: null, futureField: "additive" }))).toBe(true);
    for (const metadata of [
      undefined, null, "peer-1", [], {},
      { commandId: "peer-1", sourceChatId: "source" },
      { ...peer, commandId: "different" },
      { ...peer, commandId: " " },
      { ...peer, sourceChatId: "\n " },
      { ...peer, threadId: 5 },
      { ...peer, replyTo: "\t" },
      { ...peer, replyTo: false }
    ]) {
      expect(isPeerMessageEntry(withMetadata(metadata))).toBe(false);
    }
    expect(isPeerMessageEntry({ ...entry, id: " ", peerMessage: { ...peer, commandId: " " } })).toBe(false);
    expect(isPeerMessageEntry({ ...entry, role: "assistant" })).toBe(false);
    expect(isPeerMessageEntry({ ...entry, role: "system" })).toBe(false);
    expect(isPeerMessageEntry({ ...entry, continuationOf: "root" })).toBe(false);
    const lookalike = { ...entry, peerMessage: undefined, parts: [
      { kind: "text" as const, id: "text", text: "[Inter-session message] From session source: original user text" }
    ] };
    expect(isPeerMessageEntry(lookalike)).toBe(false);
    expect(joinContinuations([lookalike])).toEqual([lookalike]);
  });

  it("retains provenance and the full body across splits, snapshots and reconnect updates", () => {
    const source = new LoroDoc();
    const reconnect = new LoroDoc();
    const fresh = new LoroDoc();
    try {
      source.getMap("meta").set("chatId", "destination");
      const large = { ...entry, parts: [
        { kind: "text" as const, id: "first", text: "first original paragraph".repeat(3) },
        { kind: "text" as const, id: "last", text: "second original paragraph".repeat(3) }
      ] };
      const pieces = splitMessageEntry(large, 150);
      expect(pieces).toHaveLength(2);
      appendEntry(source, pieces[0]!);
      source.commit();
      reconnect.import(source.export({ mode: "snapshot" }));
      const from = reconnect.version();
      try {
        appendEntry(source, pieces[1]!);
        source.commit();
        reconnect.import(source.export({ mode: "update", from }));
      } finally {
        from.free();
      }
      fresh.import(source.export({ mode: "snapshot" }));
      for (const doc of [source, reconnect, fresh]) {
        expect(readMessageEntries(doc)).toEqual(pieces);
        const tail = materializeTail(doc, 456, 1);
        expect(tail.messages).toEqual([large]);
        expect(tail.totalMessages).toBe(1);
        expect(tail.chatId).toBe("destination");
        expect(textOf(tail.messages[0]!)).toBe(textOf(large));
        expect(isPeerMessageEntry(tail.messages[0]!)).toBe(true);
      }
    } finally {
      source.free();
      reconnect.free();
      fresh.free();
    }
  });

  it("keeps malformed, unknown and legacy metadata/content intact in materialized history", () => {
    const source = new LoroDoc();
    const restored = new LoroDoc();
    try {
      const fixtures = [
        { ...entry, peerMessage: undefined },
        withMetadata({ ...peer, threadId: 7, unknown: { payload: "preserve" } }),
        withMetadata({ ...peer, futureField: "additive" }),
        { ...entry, role: "assistant" as const },
        { ...entry, peerMessage: undefined, parts: [
          { kind: "text" as const, id: "text", text: "[Inter-session message] From session source: legacy text" }
        ] }
      ];
      for (const fixture of fixtures) appendEntry(source, fixture);
      const historicalCommand = source.getList("commands").insertContainer(0, new LoroMap());
      historicalCommand.set("id", entry.id);
      historicalCommand.set("kind", "peerMessage");
      historicalCommand.set("payload", { sourceChatId: peer.sourceChatId, threadId: peer.threadId });
      source.commit();
      restored.import(source.export({ mode: "snapshot" }));
      const messages = readMessageEntries(restored);
      expect(messages).toEqual(fixtures.map((fixture) => JSON.parse(JSON.stringify(fixture))));
      expect(materializeTail(restored, 456).messages).toEqual(messages);
      expect(messages.map(isPeerMessageEntry)).toEqual([false, false, true, false, false]);
      expect(messages.map(textOf)).toEqual(fixtures.map(textOf));
    } finally {
      source.free();
      restored.free();
    }
  });

  it("does not tuck ordinary, mixed-role or unsupported continuations under a hidden peer root", () => {
    const continuation = { ...entry, id: "peer-1#c1", continuationOf: entry.id,
      parts: [{ kind: "text" as const, id: "extra", text: "must remain discoverable" }] };
    expect(joinContinuations([entry, continuation])).toEqual([
      { ...entry, parts: [...entry.parts, ...continuation.parts] }
    ]);
    for (const child of [
      { ...continuation, peerMessage: undefined },
      { ...continuation, role: "assistant" as const },
      { ...continuation, peerMessage: { ...peer, threadId: "different" } }
    ]) {
      expect(joinContinuations([entry, child])).toEqual([entry, child]);
      expect(isPeerMessageEntry(child)).toBe(false);
    }
    expect(joinContinuations([continuation])).toEqual([continuation]);
    expect(isPeerMessageEntry(continuation)).toBe(false);
    const legacy = { ...entry, peerMessage: undefined };
    const joined = joinContinuations([legacy, continuation]);
    expect(textOf(joined[0]!)).toBe(textOf(legacy) + textOf(continuation));
    expect(isPeerMessageEntry(joined[0]!)).toBe(false);
  });
});
